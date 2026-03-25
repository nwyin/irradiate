//! Trampoline code generation: takes function mutations and produces
//! the trampolined output (orig + variants + lookup dict + wrapper).

use crate::mutation::{apply_mutation, DescriptorDecorator, FunctionMutations};

/// Unicode separator for class method name mangling (same as mutmut).
const CLASS_SEPARATOR: &str = "\u{01C1}"; // ǁ

/// Mangle a function name following mutmut convention.
pub fn mangle_name(name: &str, class_name: Option<&str>) -> String {
    if let Some(cls) = class_name {
        format!("x{CLASS_SEPARATOR}{cls}{CLASS_SEPARATOR}{name}")
    } else {
        format!("x_{name}")
    }
}

/// Result of generating a trampoline for a single function.
pub struct TrampolineOutput {
    /// Module-level code: renamed orig, mutant variants, lookup dict, __name__ assignment.
    /// These have mangled names and belong at module level.
    pub module_code: String,
    /// Wrapper function with the original name and signature.
    /// For class methods, must be indented and placed inside the class body.
    /// For top-level functions, goes at module level.
    pub wrapper_code: String,
    /// Mutant keys like "module.x_func__irradiate_1".
    pub mutant_keys: Vec<String>,
    /// If the original function had a descriptor decorator, this is the decorator
    /// line (e.g. "@property\n") that must be emitted before the wrapper.
    pub decorator_prefix: String,
}

/// Generate the full trampolined output for a single function.
pub fn generate_trampoline(fm: &FunctionMutations, module_name: &str) -> TrampolineOutput {
    let mangled = mangle_name(&fm.name, fm.class_name.as_deref());
    let mut module_lines = Vec::new();
    let mut mutant_keys = Vec::new();

    // Original function, renamed
    let orig_name = format!("{mangled}__irradiate_orig");
    let renamed_orig = rename_function(&fm.source, &fm.name, &orig_name);
    module_lines.push(renamed_orig);
    module_lines.push(String::new());

    // Mutant variants
    for (i, mutation) in fm.mutations.iter().enumerate() {
        let variant_name = format!("{mangled}__irradiate_{}", i + 1);
        let mutated_source = apply_mutation(&fm.source, mutation);
        let renamed_variant = rename_function(&mutated_source, &fm.name, &variant_name);
        module_lines.push(renamed_variant);
        module_lines.push(String::new());

        mutant_keys.push(format!("{module_name}.{variant_name}"));
    }

    // Lookup dict
    module_lines.push(format!("{mangled}__irradiate_mutants = {{"));
    for (i, _) in fm.mutations.iter().enumerate() {
        let variant_name = format!("{mangled}__irradiate_{}", i + 1);
        module_lines.push(format!("    '{variant_name}': {variant_name},"));
    }
    module_lines.push("}".to_string());

    // Set __name__ on orig for trampoline dispatch
    module_lines.push(format!("{orig_name}.__name__ = '{mangled}'"));

    // Trampoline wrapper with original name and signature.
    // The calling convention depends on the descriptor decorator:
    //   - Regular instance method:  pass self; look up via _type(self). for MRO-correct access.
    //   - @classmethod:             pass cls; look up via cls.
    //   - @staticmethod:            pass None; look up via ClassName.
    //   - @property:                pass self; look up via _type(self). (same as instance method)
    //   - Top-level function:       pass None; bare names in module scope.
    // Extract the actual first parameter name (handles mcs, cls, self, etc.)
    let first_param_name = extract_first_param_name(&fm.params_source);
    let (self_arg, has_self, lookup_prefix) = match (fm.class_name.as_deref(), fm.descriptor_decorator) {
        (Some(_), Some(DescriptorDecorator::ClassMethod)) => {
            let name = first_param_name.as_deref().unwrap_or("cls");
            (name, true, format!("{name}."))
        }
        (Some(cls), Some(DescriptorDecorator::StaticMethod)) => ("None", false, format!("{cls}.")),
        (Some(_), _) => {
            let name = first_param_name.as_deref().unwrap_or("self");
            (name, true, format!("_type({name})."))
        }
        (None, _) => ("None", false, String::new()), // top-level function
    };
    let params_text = &fm.params_source;
    let wrapper_code = generate_wrapper_function(
        &fm.name,
        &mangled,
        params_text,
        self_arg,
        has_self,
        &lookup_prefix,
        fm.is_async,
        fm.is_generator,
    );

    let decorator_prefix = match fm.descriptor_decorator {
        Some(DescriptorDecorator::Property) => "@property\n".to_string(),
        Some(DescriptorDecorator::ClassMethod) => "@classmethod\n".to_string(),
        Some(DescriptorDecorator::StaticMethod) => "@staticmethod\n".to_string(),
        None => String::new(),
    };

    TrampolineOutput {
        module_code: module_lines.join("\n"),
        wrapper_code,
        mutant_keys,
        decorator_prefix,
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_wrapper_function(
    original_name: &str,
    mangled_name: &str,
    params_source: &str,
    self_arg: &str,
    has_self: bool,
    lookup_prefix: &str,
    is_async: bool,
    is_generator: bool,
) -> String {
    let async_prefix = if is_async && !is_generator { "async " } else { "" };

    // Parse parameter names from the params source to build forwarding args.
    // has_self=true strips the implicit first parameter (self or cls) from call_args.
    let (pos_args, kw_args, kwargs_name) = parse_param_names(params_source, has_self);

    let args_list = if pos_args.is_empty() {
        "()".to_string()
    } else {
        format!(
            "({}{})",
            pos_args.join(", "),
            if pos_args.len() == 1 { "," } else { "" }
        )
    };

    // Build call_kwargs dict, merging **kwargs if present.
    let kwargs_dict = if kw_args.is_empty() && kwargs_name.is_none() {
        "{}".to_string()
    } else {
        let mut entries: Vec<String> = kw_args.iter().map(|k| format!("'{k}': {k}")).collect();
        if let Some(ref kn) = kwargs_name {
            entries.push(format!("**{kn}"));
        }
        format!("{{{}}}", entries.join(", "))
    };

    // lookup_prefix controls how mangled names are resolved:
    //   instance method  → "_type(self)." (class attribute via MRO; uses module-level
    //                       _type=type alias to avoid shadowing by parameter names like `type`)
    //   classmethod      → "cls."        (cls IS the class)
    //   staticmethod     → "ClassName."  (no implicit arg; use class name directly)
    //   top-level fn     → ""            (module globals are directly accessible)
    let trampoline_call = format!(
        "_irradiate_trampoline({lookup_prefix}{mangled_name}__irradiate_orig, {lookup_prefix}{mangled_name}__irradiate_mutants, {args_list}, {kwargs_dict}, {self_arg})"
    );

    // Choose the correct dispatch based on function kind:
    //   async generator  → return trampoline(...)  (plain def, returns async gen object directly)
    //   sync generator   → yield from trampoline(...)
    //   async regular    → return await trampoline(...)
    //   sync regular     → return trampoline(...)
    let body = match (is_async, is_generator) {
        (true, true) => format!("    return {trampoline_call}"),
        (false, true) => format!("    yield from {trampoline_call}"),
        (true, false) => format!("    return await {trampoline_call}"),
        (false, false) => format!("    return {trampoline_call}"),
    };

    // Strip type annotations from the wrapper def line.  The wrapper is a pure
    // dispatcher — annotations serve no purpose and can cause NameError when
    // forward-referenced types aren't in the trampolined module's namespace (#26).
    // The _orig function retains the full annotated signature.
    let bare_params = strip_annotations_from_params(params_source);
    format!("{async_prefix}def {original_name}({bare_params}):\n{body}")
}

/// Strip inline comments from a params source string (line by line).
/// This handles `# type: ignore[override]` and similar annotations.
/// Must be string-aware: `fill_char: str = "#"` is NOT a comment.
/// Extract the first parameter name from a Python parameter list string.
/// Handles type annotations and defaults: "mcs, klass: type" → Some("mcs").
fn extract_first_param_name(params_source: &str) -> Option<String> {
    // Strip inline comments first (handles `# type: ignore[override]` on def line)
    let cleaned = strip_inline_comments(params_source);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Take text before first comma or end, then strip annotation/default
    let first = trimmed.split(',').next().unwrap_or(trimmed).trim();
    // Skip positional-only separator
    if first == "/" {
        return None;
    }
    let name = first
        .split(':')
        .next()
        .unwrap_or(first)
        .split('=')
        .next()
        .unwrap_or(first)
        .trim();
    if name.is_empty() { None } else { Some(name.to_string()) }
}

fn strip_inline_comments(s: &str) -> String {
    s.lines()
        .map(|line| {
            // Find the first `#` that is NOT inside a string literal.
            let mut in_single = false;
            let mut in_double = false;
            let mut escape = false;
            for (i, ch) in line.char_indices() {
                if escape {
                    escape = false;
                    continue;
                }
                match ch {
                    '\\' => escape = true,
                    '\'' if !in_double => in_single = !in_single,
                    '"' if !in_single => in_double = !in_double,
                    '#' if !in_single && !in_double => return &line[..i],
                    _ => {}
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Split a parameter list string by commas, respecting bracket nesting.
/// Only splits on commas at bracket depth 0 (not inside `[`, `(`, or `{`).
fn split_params(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth: i32 = 0;

    for ch in s.chars() {
        match ch {
            '[' | '(' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ']' | ')' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(current.trim().to_string());
                current = String::new();
            }
            _ => {
                current.push(ch);
            }
        }
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        parts.push(trimmed);
    }

    parts
}

/// Find the first occurrence of `sep` at bracket depth 0.
/// Returns `(before, Some(after))` or `(whole_string, None)`.
fn split_at_depth0(s: &str, sep: char) -> (&str, Option<&str>) {
    let mut depth: i32 = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            c if c == sep && depth == 0 => {
                return (&s[..i], Some(&s[i + ch.len_utf8()..]));
            }
            _ => {}
        }
    }
    (s, None)
}

/// Strip type annotations from a parameter list, preserving names and defaults.
/// The wrapper function is just a dispatcher — annotations only cause NameErrors
/// when forward-referenced types aren't in scope (issue #26).
///
/// Examples:
///   "x: int, y: str = 'hi'"          → "x, y='hi'"
///   "self, x: Dict[str, int] = {}"   → "self, x={}"
///   "*, key: str = 'a', **kw: Any"   → "*, key='a', **kw"
///   "*args: int"                      → "*args"
fn strip_annotations_from_params(params_source: &str) -> String {
    let stripped = strip_inline_comments(params_source);
    let parts = split_params(&stripped);
    let mut result = Vec::new();

    for part in &parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // Keep separators as-is
        if part == "*" || part == "/" {
            result.push(part.to_string());
            continue;
        }

        // **kwargs or *args: strip annotation after name
        if part.starts_with("**") || part.starts_with('*') {
            let prefix = if part.starts_with("**") { "**" } else { "*" };
            let rest = &part[prefix.len()..];
            let (name_part, _) = split_at_depth0(rest, ':');
            result.push(format!("{}{}", prefix, name_part.trim()));
            continue;
        }

        // Regular param: strip `: annotation`, keep `= default`
        let (name_part, after_colon) = split_at_depth0(part, ':');
        let name = name_part.trim();

        if let Some(after) = after_colon {
            // Has annotation — look for default after it
            let (_, default_part) = split_at_depth0(after, '=');
            if let Some(default) = default_part {
                result.push(format!("{}={}", name, default.trim()));
            } else {
                result.push(name.to_string());
            }
        } else {
            // No annotation — keep as-is (may have = default)
            result.push(part.to_string());
        }
    }

    result.join(", ")
}

/// Parse parameter names from a params source string.
/// Returns (positional_args, keyword_only_args, kwargs_name).
pub fn parse_param_names(
    params_source: &str,
    has_self: bool,
) -> (Vec<String>, Vec<String>, Option<String>) {
    let mut pos_args = Vec::new();
    let mut kw_args = Vec::new();
    let mut kwargs_name: Option<String> = None;
    let mut after_star = false;

    // Strip inline comments before splitting, then do bracket-aware split.
    let stripped = strip_inline_comments(params_source);

    for part in split_params(&stripped) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // Handle bare * separator
        if part == "*" || part == "/" {
            if part == "*" {
                after_star = true;
            }
            continue;
        }

        // Handle **kwargs
        if part.starts_with("**") {
            let name = part
                .trim_start_matches('*')
                .split(':')
                .next()
                .unwrap_or("")
                .trim();
            if !name.is_empty() {
                kwargs_name = Some(name.to_string());
            }
            continue;
        }

        // Handle *args
        if part.starts_with('*') {
            after_star = true;
            // *args — include as starred
            let name = part
                .trim_start_matches('*')
                .split(':')
                .next()
                .unwrap_or("")
                .split('=')
                .next()
                .unwrap_or("")
                .trim();
            if !name.is_empty() {
                pos_args.push(format!("*{name}"));
            }
            continue;
        }

        // Extract just the parameter name (before : or =)
        let name = part
            .split(':')
            .next()
            .unwrap_or(part)
            .split('=')
            .next()
            .unwrap_or(part)
            .trim();
        if name.is_empty() {
            continue;
        }

        if after_star {
            kw_args.push(name.to_string());
        } else {
            pos_args.push(name.to_string());
        }
    }

    // Strip the implicit first argument (self for instance methods, cls for classmethods).
    // We strip by position rather than by name so that both `self` and `cls` are handled.
    if has_self && !pos_args.is_empty() {
        pos_args.remove(0);
    }

    (pos_args, kw_args, kwargs_name)
}

/// Rename a function definition by replacing the function name.
fn rename_function(source: &str, old_name: &str, new_name: &str) -> String {
    // Find "def old_name(" and replace with "def new_name("
    let pattern = format!("def {old_name}(");
    let replacement = format!("def {new_name}(");
    // Only replace the first occurrence (the function definition line)
    if let Some(pos) = source.find(&pattern) {
        format!(
            "{}{}{}",
            &source[..pos],
            replacement,
            &source[pos + pattern.len()..]
        )
    } else {
        // Try async def
        let pattern = format!("async def {old_name}(");
        let replacement = format!("async def {new_name}(");
        if let Some(pos) = source.find(&pattern) {
            format!(
                "{}{}{}",
                &source[..pos],
                replacement,
                &source[pos + pattern.len()..]
            )
        } else {
            source.to_string()
        }
    }
}

/// Generate the trampoline implementation that gets prepended to mutated files.
pub fn trampoline_impl() -> &'static str {
    r#"import irradiate_harness as _ih
_type = type


def _irradiate_trampoline(orig, mutants, call_args, call_kwargs, self_arg=None, args=None):
    active = _ih.active_mutant
    if not active:
        if self_arg is not None:
            return orig(self_arg, *call_args, **call_kwargs)
        return orig(*call_args, **call_kwargs) if call_args is not None else None
    if active == 'fail':
        raise _ih.ProgrammaticFailException()
    if active == 'stats':
        _ih.record_hit(orig.__module__ + '.' + orig.__name__)
        if self_arg is not None:
            return orig(self_arg, *call_args, **call_kwargs)
        return orig(*call_args, **call_kwargs) if call_args is not None else None
    prefix = orig.__module__ + '.' + orig.__name__ + '__irradiate_'
    if not active.startswith(prefix):
        if self_arg is not None:
            return orig(self_arg, *call_args, **call_kwargs)
        return orig(*call_args, **call_kwargs) if call_args is not None else None
    variant = active.rpartition('.')[-1]
    if self_arg is not None:
        return mutants[variant](self_arg, *call_args, **call_kwargs)
    return mutants[variant](*call_args, **call_kwargs)
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutation::collect_file_mutations;

    #[test]
    fn test_mangle_name_top_level() {
        assert_eq!(mangle_name("hello", None), "x_hello");
    }

    #[test]
    fn test_mangle_name_class_method() {
        assert_eq!(mangle_name("bar", Some("Foo")), "x\u{01C1}Foo\u{01C1}bar");
    }

    #[test]
    fn test_rename_function() {
        let source = "def add(a, b):\n    return a + b\n";
        let renamed = rename_function(source, "add", "x_add__irradiate_orig");
        assert!(renamed.starts_with("def x_add__irradiate_orig("));
        assert!(renamed.contains("return a + b"));
    }

    #[test]
    fn test_generate_trampoline() {
        let source = "def add(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());

        let output = generate_trampoline(&fms[0], "my_lib");
        assert!(
            output.module_code.contains("x_add__irradiate_orig"),
            "Should have renamed original"
        );
        assert!(
            output.module_code.contains("x_add__irradiate_1"),
            "Should have at least one variant"
        );
        assert!(
            output.module_code.contains("x_add__irradiate_mutants"),
            "Should have lookup dict"
        );
        assert!(
            output.wrapper_code.contains("def add("),
            "Should have trampoline wrapper"
        );
        assert!(!output.mutant_keys.is_empty(), "Should produce mutant keys");
        assert!(
            output.mutant_keys[0].starts_with("my_lib.x_add__irradiate_"),
            "Keys should be module-qualified"
        );
    }

    // INV-1: Parameters with generic type annotations parse to the correct name only.
    #[test]
    fn test_parse_param_names_generic_annotation() {
        let (pos_args, kw_args, kwargs) =
            parse_param_names("self, mapping: cabc.Mapping[str, t.Any], /", true);
        assert_eq!(pos_args, vec!["mapping"]);
        assert_eq!(kw_args, Vec::<String>::new());
        assert_eq!(kwargs, None);
    }

    // INV-2: Inline comments do not affect parameter extraction.
    #[test]
    fn test_parse_param_names_inline_comment() {
        let (pos_args, kw_args, kwargs) = parse_param_names(
            "self,\n    mapping: cabc.Mapping[str, t.Any],  # type: ignore[override]\n    /,",
            true,
        );
        assert_eq!(pos_args, vec!["mapping"]);
        assert_eq!(kw_args, Vec::<String>::new());
        assert_eq!(kwargs, None);
    }

    // INV-3: Nested brackets parse correctly.
    #[test]
    fn test_parse_param_names_nested_brackets() {
        let (pos_args, kw_args, kwargs) =
            parse_param_names("self, x: Dict[str, List[int]], y: int", true);
        assert_eq!(pos_args, vec!["x", "y"]);
        assert_eq!(kw_args, Vec::<String>::new());
        assert_eq!(kwargs, None);
    }

    // INV-4: Existing simple-param behavior unchanged.
    #[test]
    fn test_parse_param_names_simple() {
        let (pos_args, kw_args, kwargs) = parse_param_names("a, b, c", false);
        assert_eq!(pos_args, vec!["a", "b", "c"]);
        assert_eq!(kw_args, Vec::<String>::new());
        assert_eq!(kwargs, None);
    }

    // Tuple with ellipsis and keyword-only args after *.
    #[test]
    fn test_parse_param_names_tuple_kwonly() {
        let (pos_args, kw_args, kwargs) =
            parse_param_names("self, x: Tuple[int, ...], *, key: str", true);
        assert_eq!(pos_args, vec!["x"]);
        assert_eq!(kw_args, vec!["key"]);
        assert_eq!(kwargs, None);
    }

    // Multiple bracket types: Dict[str, Tuple[int, ...]].
    #[test]
    fn test_parse_param_names_deeply_nested() {
        let (pos_args, kw_args, kwargs) =
            parse_param_names("self, x: Dict[str, Tuple[int, ...]], y: int", true);
        assert_eq!(pos_args, vec!["x", "y"]);
        assert_eq!(kw_args, Vec::<String>::new());
        assert_eq!(kwargs, None);
    }

    // Positional-only separator after bracketed annotation.
    #[test]
    fn test_parse_param_names_pos_only_after_bracket() {
        let (pos_args, kw_args, kwargs) =
            parse_param_names("self, mapping: Mapping[str, Any], /", true);
        assert_eq!(pos_args, vec!["mapping"]);
        assert_eq!(kw_args, Vec::<String>::new());
        assert_eq!(kwargs, None);
    }

    // INV: **kwargs is captured and excluded from kw_args.
    #[test]
    fn test_parse_param_names_kwargs() {
        let (pos_args, kw_args, kwargs) = parse_param_names("a, /, b, *, c, **kwargs", false);
        assert_eq!(pos_args, vec!["a", "b"]);
        assert_eq!(kw_args, vec!["c"]);
        assert_eq!(kwargs, Some("kwargs".to_string()));
    }

    // INV: **kwargs is merged into call_kwargs in the wrapper.
    #[test]
    fn test_wrapper_kwargs_forwarding() {
        let wrapper = generate_wrapper_function(
            "func_with_star",
            "x_func_with_star",
            "a, /, b, *, c, **kwargs",
            "None",
            false,
            "",
            false,
            false,
        );
        // kwargs must be spread into the call_kwargs dict
        assert!(
            wrapper.contains("**kwargs"),
            "wrapper must forward **kwargs: {wrapper}"
        );
        assert!(
            wrapper.contains("'c': c"),
            "wrapper must include c in call_kwargs: {wrapper}"
        );
    }

    // INV-1: Wrapper strips type annotations from def line (#26).
    #[test]
    fn test_wrapper_strips_annotations() {
        let wrapper = generate_wrapper_function(
            "some_func",
            "x_some_func",
            "a, b: str = \"111\"",
            "None",
            false,
            "",
            false,
            false,
        );
        assert!(
            wrapper.starts_with("def some_func(a, b=\"111\"):"),
            "wrapper must strip annotations: {wrapper}"
        );
    }

    // INV-2: Wrapper does not include return annotation (#26).
    #[test]
    fn test_wrapper_strips_return_annotation() {
        let wrapper = generate_wrapper_function(
            "some_func",
            "x_some_func",
            "a: int, b: str = \"111\"",
            "None",
            false,
            "",
            false,
            false,
        );
        assert!(
            !wrapper.contains("->"),
            "wrapper must not include return annotation: {wrapper}"
        );
        assert!(
            wrapper.starts_with("def some_func(a, b=\"111\"):"),
            "wrapper def line: {wrapper}"
        );
    }

    // INV-3: Wrapper without annotations or kwargs still correct.
    #[test]
    fn test_wrapper_no_annotation_no_kwargs() {
        let wrapper =
            generate_wrapper_function("add", "x_add", "a, b", "None", false, "", false, false);
        assert!(
            wrapper.starts_with("def add(a, b):"),
            "wrapper def line must be clean: {wrapper}"
        );
    }

    // INV: generate_trampoline strips annotations from wrapper (#26).
    #[test]
    fn test_generate_trampoline_strips_annotations() {
        let source =
            "def some_func(a, b: str = \"111\", c: int = 0) -> int | None:\n    return a + c\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should find mutations in some_func");
        let output = generate_trampoline(&fms[0], "my_lib");
        assert!(
            !output.wrapper_code.contains("-> int | None"),
            "wrapper must NOT include return annotation: {}",
            output.wrapper_code
        );
        assert!(
            output.wrapper_code.contains("def some_func(a, b=\"111\", c=0):"),
            "wrapper must strip param annotations: {}",
            output.wrapper_code
        );
    }

    // --- strip_annotations_from_params unit tests ---

    #[test]
    fn test_strip_annotations_simple() {
        assert_eq!(strip_annotations_from_params("x: int, y: str"), "x, y");
    }

    #[test]
    fn test_strip_annotations_with_defaults() {
        assert_eq!(
            strip_annotations_from_params("x: int = 5, y: str = \"hi\""),
            "x=5, y=\"hi\""
        );
    }

    #[test]
    fn test_strip_annotations_no_annotations() {
        assert_eq!(strip_annotations_from_params("a, b, c"), "a, b, c");
    }

    #[test]
    fn test_strip_annotations_self_and_typed() {
        assert_eq!(
            strip_annotations_from_params("self, x: Dict[str, int] = {}"),
            "self, x={}"
        );
    }

    #[test]
    fn test_strip_annotations_nested_generics() {
        assert_eq!(
            strip_annotations_from_params("x: Dict[str, List[int]], y: Tuple[int, ...]"),
            "x, y"
        );
    }

    #[test]
    fn test_strip_annotations_star_separator() {
        assert_eq!(
            strip_annotations_from_params("x: int, /, y: str, *, key: str = \"a\""),
            "x, /, y, *, key=\"a\""
        );
    }

    #[test]
    fn test_strip_annotations_args_kwargs() {
        assert_eq!(
            strip_annotations_from_params("*args: Any, **kwargs: Any"),
            "*args, **kwargs"
        );
    }

    #[test]
    fn test_strip_annotations_forward_ref() {
        // This is the tinygrad case — uint32_t is a forward reference
        assert_eq!(
            strip_annotations_from_params("self, x: uint32_t, y: uint32_t = 0"),
            "self, x, y=0"
        );
    }

    #[test]
    fn test_strip_annotations_callable() {
        assert_eq!(
            strip_annotations_from_params("fn: Callable[[int, str], bool], x: int"),
            "fn, x"
        );
    }

    // INV-1/INV-2: All three passthrough paths in trampoline_impl must forward self_arg.
    // Regression test: before fix, the inactive/stats/prefix-mismatch paths called
    // orig(*call_args) without prepending self_arg, causing TypeError for class methods.
    #[test]
    fn test_trampoline_impl_all_passthrough_paths_forward_self_arg() {
        let impl_str = trampoline_impl();
        // 3 passthrough paths call orig(self_arg, ...) — inactive, stats, prefix-mismatch
        let orig_self_count = impl_str.matches("orig(self_arg, *call_args").count();
        assert_eq!(
            orig_self_count, 3,
            "All 3 passthrough paths (inactive, stats, prefix-mismatch) must forward self_arg to orig: found {orig_self_count}"
        );
        // Mutant dispatch path calls mutants[variant](self_arg, ...)
        assert!(
            impl_str.contains("mutants[variant](self_arg, *call_args"),
            "Mutant dispatch path must also forward self_arg"
        );
    }

    // INV-3: Top-level trampoline wrapper uses None as self_arg.
    #[test]
    fn test_trampoline_wrapper_top_level_uses_none_self_arg() {
        let source = "def add(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let output = generate_trampoline(&fms[0], "my_lib");
        assert!(
            output.wrapper_code.contains(", None)"),
            "Top-level wrapper should pass None as self_arg; got: {}",
            output.wrapper_code
        );
    }

    // INV-1: Class method trampoline wrapper passes `self` as self_arg.
    #[test]
    fn test_trampoline_wrapper_class_method_uses_self_arg() {
        let source =
            "class Point:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n";
        let fms = collect_file_mutations(source);
        let class_fm = fms
            .iter()
            .find(|fm| fm.class_name.is_some())
            .expect("should find class method");
        let output = generate_trampoline(class_fm, "point_module");
        assert!(
            output.wrapper_code.contains(", self)"),
            "Class method wrapper should pass self as self_arg; got: {}",
            output.wrapper_code
        );
    }

    // INV-1: Class method wrapper must use `_type(self).` prefix for mangled name lookups.
    // Without this, Python raises NameError because class body names are NOT in scope
    // for methods — they are class attributes, not locals or globals.
    #[test]
    fn test_class_method_wrapper_uses_type_self_lookup() {
        let source =
            "class Point:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n";
        let fms = collect_file_mutations(source);
        let class_fm = fms
            .iter()
            .find(|fm| fm.class_name.is_some())
            .expect("should find class method");
        let output = generate_trampoline(class_fm, "point_module");
        assert!(
            output.wrapper_code.contains("_type(self)."),
            "Class method wrapper must use _type(self). prefix for mangled lookups; got:\n{}",
            output.wrapper_code
        );
        // Should NOT use bare mangled name (would NameError at runtime)
        let mangled = mangle_name("__init__", Some("Point"));
        let bare_orig = format!("{mangled}__irradiate_orig");
        assert!(
            !output.wrapper_code.contains(&format!("({bare_orig},")),
            "Class method wrapper must NOT use bare mangled orig (would NameError); got:\n{}",
            output.wrapper_code
        );
    }

    // INV-2: Inheritance works — type(self) uses MRO so Child inheriting from Point
    // uses Child's class dict first (which inherits Point's mangled attrs).
    // This is verified by checking `_type(self).` is used rather than `Point.` (hardcoded).
    #[test]
    fn test_class_method_wrapper_not_hardcoded_class_name() {
        let source = "class MyClass:\n    def method(self, v):\n        return v + 1\n";
        let fms = collect_file_mutations(source);
        let class_fm = fms
            .iter()
            .find(|fm| fm.class_name.is_some())
            .expect("should find class method");
        let output = generate_trampoline(class_fm, "mod");
        // _type(self). is used — not the literal class name
        assert!(
            output.wrapper_code.contains("_type(self)."),
            "Class method wrapper must use _type(self). not hardcoded class name; got:\n{}",
            output.wrapper_code
        );
        assert!(
            !output.wrapper_code.contains("MyClass.x"),
            "Class method wrapper must use _type(self). not 'MyClass.x'; got:\n{}",
            output.wrapper_code
        );
    }

    // INV-3: Top-level function wrapper still uses bare names (no _type(self). prefix).
    #[test]
    fn test_top_level_wrapper_no_type_self_prefix() {
        let source = "def add(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let output = generate_trampoline(&fms[0], "my_lib");
        assert!(
            !output.wrapper_code.contains("_type(self)."),
            "Top-level wrapper must NOT use _type(self). prefix; got:\n{}",
            output.wrapper_code
        );
    }

    // INV-2: Generator wrapper uses `yield from` instead of `return`.
    // Uses `if n > 0: yield n` to guarantee a compop mutation (so the function is collected).
    #[test]
    fn test_generator_wrapper_uses_yield_from() {
        let source = "def gen(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        assert!(
            !fms.is_empty(),
            "generator with compop must produce mutations"
        );
        let output = generate_trampoline(&fms[0], "gen_mod");
        assert!(
            output
                .wrapper_code
                .contains("yield from _irradiate_trampoline("),
            "Generator wrapper must use 'yield from', got:\n{}",
            output.wrapper_code
        );
        assert!(
            !output.wrapper_code.contains("return "),
            "Generator wrapper must NOT use 'return', got:\n{}",
            output.wrapper_code
        );
    }

    // INV-1: Async generator wrapper returns trampoline result directly (no nested async gen).
    // INV-2: Async generator wrapper uses plain `def` (not `async def`).
    // INV-3: Async generator wrapper has no yield, async for, or aclose.
    #[test]
    fn test_async_generator_wrapper_returns_directly() {
        let source = "async def agen(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        assert!(
            !fms.is_empty(),
            "async generator with compop must produce mutations"
        );
        let output = generate_trampoline(&fms[0], "agen_mod");
        // INV-1: must use plain return (pass through async gen object)
        assert!(
            output.wrapper_code.contains("return _irradiate_trampoline("),
            "Async generator wrapper must return trampoline result directly, got:\n{}",
            output.wrapper_code
        );
        // INV-2: must be plain def (not async def)
        assert!(
            output.wrapper_code.starts_with("def agen("),
            "Async generator wrapper must be plain def (not async def), got:\n{}",
            output.wrapper_code
        );
        // INV-3: must NOT contain yield, async for, or aclose
        assert!(
            !output.wrapper_code.contains("yield"),
            "Async generator wrapper must NOT contain yield, got:\n{}",
            output.wrapper_code
        );
        assert!(
            !output.wrapper_code.contains("async for"),
            "Async generator wrapper must NOT contain 'async for', got:\n{}",
            output.wrapper_code
        );
        assert!(
            !output.wrapper_code.contains("aclose"),
            "Async generator wrapper must NOT contain 'aclose', got:\n{}",
            output.wrapper_code
        );
    }

    // INV-4: Regular async function still uses `return await` (no regression).
    #[test]
    fn test_async_regular_wrapper_uses_return_await() {
        let source = "async def fetch(url):\n    return url + 'x'\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let output = generate_trampoline(&fms[0], "fetch_mod");
        assert!(
            output
                .wrapper_code
                .contains("return await _irradiate_trampoline("),
            "Async regular wrapper must use 'return await', got:\n{}",
            output.wrapper_code
        );
    }

    // INV-5: Regular sync function still uses `return` (no regression).
    #[test]
    fn test_sync_regular_wrapper_uses_return() {
        let source = "def add(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let output = generate_trampoline(&fms[0], "math_mod");
        assert!(
            output
                .wrapper_code
                .contains("return _irradiate_trampoline("),
            "Sync regular wrapper must use 'return', got:\n{}",
            output.wrapper_code
        );
        assert!(
            !output.wrapper_code.contains("yield"),
            "Sync regular wrapper must NOT use 'yield', got:\n{}",
            output.wrapper_code
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Decorator skip invariants (INV-1, INV-2, INV-3)
    // ─────────────────────────────────────────────────────────────────

    // INV-1: Any function with one or more decorators produces NO mutations.
    #[test]
    fn test_non_descriptor_decorator_skips_function() {
        let cases = [
            "@contextmanager\ndef ctx():\n    yield 1 + 2\n",
            "@custom_decorator\ndef qux(a, b):\n    return a + b\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            assert!(
                fms.is_empty(),
                "non-descriptor decorated function must produce no mutations; source:\n{source}\ngot: {fms:?}"
            );
        }
    }

    #[test]
    fn test_descriptor_decorator_collected() {
        // Descriptor decorators on class methods should produce mutations.
        let cases = [
            "class C:\n    @property\n    def x(self):\n        return self._x\n",
            "class C:\n    @classmethod\n    def make(cls):\n        return 1 + 2\n",
            "class C:\n    @staticmethod\n    def helper():\n        return 1 + 2\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            assert!(
                !fms.is_empty(),
                "descriptor-decorated function must produce mutations; source:\n{source}"
            );
        }
    }

    // INV-1: Stacked decorators — any decorator on the function triggers the skip.
    #[test]
    fn test_stacked_decorators_skipped() {
        let source = "@decorator1\n@decorator2\ndef stacked(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "function with stacked decorators must produce no mutations");
    }

    // INV-2: Functions WITHOUT decorators still produce mutations (regression).
    #[test]
    fn test_plain_function_still_mutated() {
        let source = "def plain(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "plain (undecorated) function must still produce mutations");
    }

    // INV-3: Class with mix of decorated and undecorated methods — only undecorated produces mutations.
    #[test]
    fn test_class_mixed_decorated_undecorated() {
        let source = concat!(
            "class Foo:\n",
            "    @classmethod\n",
            "    def make(cls):\n",
            "        return 1 + 2\n",
            "\n",
            "    def plain(self, v):\n",
            "        return v + 1\n",
        );
        let fms = collect_file_mutations(source);
        // Both should be collected — @classmethod is a descriptor decorator.
        assert_eq!(fms.len(), 2, "both classmethod and plain method should produce mutations; got: {fms:?}");
    }

    // INV-3a: @classmethod wrapper uses cls. prefix and passes cls.
    #[test]
    fn test_classmethod_wrapper_uses_cls_prefix() {
        let source = "class Foo:\n    @classmethod\n    def make(cls, n):\n        return n + 1\n";
        let fms = collect_file_mutations(source);
        let fm = fms.iter().find(|f| f.name == "make").expect("should find make");
        let output = generate_trampoline(fm, "mod");
        assert!(
            output.wrapper_code.contains("cls."),
            "@classmethod wrapper must use cls. prefix; got:\n{}",
            output.wrapper_code
        );
        assert!(
            output.wrapper_code.contains(", cls)"),
            "@classmethod wrapper must pass cls to trampoline; got:\n{}",
            output.wrapper_code
        );
        assert_eq!(output.decorator_prefix, "@classmethod\n");
    }

    // INV-3a2: @classmethod with non-standard first param (e.g. mcs for metaclasses)
    #[test]
    fn test_classmethod_wrapper_uses_actual_first_param_name() {
        let source = "class Meta:\n    @classmethod\n    def resolve(mcs, klass):\n        return klass\n";
        let fms = collect_file_mutations(source);
        let fm = fms.iter().find(|f| f.name == "resolve").expect("should find resolve");
        let output = generate_trampoline(fm, "mod");
        assert!(
            output.wrapper_code.contains("mcs."),
            "@classmethod with mcs param must use mcs. prefix; got:\n{}",
            output.wrapper_code
        );
        assert!(
            output.wrapper_code.contains(", mcs)"),
            "@classmethod with mcs param must pass mcs to trampoline; got:\n{}",
            output.wrapper_code
        );
    }

    // INV-3b: @staticmethod wrapper uses ClassName. prefix and passes None.
    #[test]
    fn test_staticmethod_wrapper_uses_classname_prefix() {
        let source = "class Foo:\n    @staticmethod\n    def helper(x):\n        return x + 1\n";
        let fms = collect_file_mutations(source);
        let fm = fms.iter().find(|f| f.name == "helper").expect("should find helper");
        let output = generate_trampoline(fm, "mod");
        assert!(
            output.wrapper_code.contains("Foo."),
            "@staticmethod wrapper must use ClassName. prefix; got:\n{}",
            output.wrapper_code
        );
        assert!(
            output.wrapper_code.contains(", None)"),
            "@staticmethod wrapper must pass None (no self/cls); got:\n{}",
            output.wrapper_code
        );
        assert_eq!(output.decorator_prefix, "@staticmethod\n");
    }

    // INV-3c: @property wrapper uses _type(self). prefix (same as instance method).
    #[test]
    fn test_property_wrapper_uses_type_self_prefix() {
        let source = "class Foo:\n    @property\n    def name(self):\n        return self._name\n";
        let fms = collect_file_mutations(source);
        let fm = fms.iter().find(|f| f.name == "name").expect("should find name");
        let output = generate_trampoline(fm, "mod");
        assert!(
            output.wrapper_code.contains("_type(self)."),
            "@property wrapper must use _type(self). prefix; got:\n{}",
            output.wrapper_code
        );
        assert_eq!(output.decorator_prefix, "@property\n");
    }

    // INV-3: Instance method without decorator uses _type(self). prefix (regression check).
    #[test]
    fn test_instance_method_still_uses_type_self_prefix() {
        let source = "class Foo:\n    def method(self, v):\n        return v + 1\n";
        let fms = collect_file_mutations(source);
        let fm = fms.iter().find(|f| f.name == "method").expect("should find method");
        let output = generate_trampoline(fm, "foo_module");
        assert!(
            output.wrapper_code.contains("_type(self)."),
            "instance method must still use _type(self). prefix; got:\n{}",
            output.wrapper_code
        );
        assert!(
            output.wrapper_code.contains(", self)"),
            "instance method must pass self to trampoline; got:\n{}",
            output.wrapper_code
        );
    }

    // Failure mode: file with only decorated functions produces empty mutation list (no crash).
    #[test]
    fn test_all_decorated_file_no_crash() {
        // Top-level @property/@classmethod without a class context — these are unusual
        // but shouldn't crash. They won't be collected as descriptor decorators because
        // they're top-level (no class_name), and our descriptor handling only applies
        // within class bodies. At top level, decorated_definition is checked and the
        // function is collected if it has only descriptor decorators.
        let source = concat!(
            "@property\n",
            "def foo(self):\n",
            "    return self._x\n",
            "\n",
            "@classmethod\n",
            "def bar(cls):\n",
            "    return 1\n",
        );
        // Top-level descriptor-decorated functions are collected (unusual but valid).
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "top-level descriptor-decorated functions should be collected");
    }

    // ─────────────────────────────────────────────────────────────────
    // Property-based tests (proptest)
    // ─────────────────────────────────────────────────────────────────

    use proptest::prelude::*;

    proptest! {
        // Target 2, P1: parse_param_names never panics on arbitrary input.
        #[test]
        fn prop_parse_param_names_no_panic(s in ".*") {
            let _ = parse_param_names(&s, false);
            let _ = parse_param_names(&s, true);
        }

        // Target 2, P2: extracted names are valid Python identifiers.
        // Uses simple regex-generated names to avoid bracket/annotation complexity.
        #[test]
        fn prop_parse_param_names_valid_identifiers(
            names in proptest::collection::vec("[a-z][a-z0-9_]{0,5}", 0..6usize),
        ) {
            let params = names.join(", ");
            let (pos_args, kw_args, kwargs_name) = parse_param_names(&params, false);
            let is_valid_ident = |s: &str| {
                !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
            };
            for arg in pos_args.iter().chain(kw_args.iter()) {
                // *args entries have a leading '*'
                let name = arg.trim_start_matches('*');
                prop_assert!(is_valid_ident(name), "invalid identifier: {arg:?}");
            }
            if let Some(kn) = &kwargs_name {
                prop_assert!(is_valid_ident(kn), "invalid kwargs name: {kn:?}");
            }
        }

        // Target 2, P3: total extracted param count <= (number of commas in input) + 1.
        // Every structural comma produces at most one parameter; skipped tokens (*, /)
        // only reduce the count, never exceed it.
        #[test]
        fn prop_parse_param_names_count_bound(
            names in proptest::collection::vec("[a-z][a-z0-9_]{0,5}", 0..8usize),
        ) {
            let params = names.join(", ");
            let (pos_args, kw_args, kwargs_name) = parse_param_names(&params, false);
            let total = pos_args.len() + kw_args.len() + usize::from(kwargs_name.is_some());
            let commas = params.chars().filter(|&c| c == ',').count();
            prop_assert!(
                total <= commas + 1,
                "params={params:?}, total={total}, commas={commas}"
            );
        }

        // Target 3, P1: generate_trampoline never panics on valid function mutations.
        #[test]
        fn prop_generate_trampoline_no_panic(
            func in prop_oneof![Just("foo"), Just("bar"), Just("compute"), Just("check_val")],
            a    in prop_oneof![Just("a"),   Just("x"),   Just("lhs"),     Just("value")],
            b    in prop_oneof![Just("b"),   Just("y"),   Just("rhs"),     Just("other")],
            op   in prop_oneof![Just("+"),   Just("-"),   Just("*"),       Just("//")],
        ) {
            let source = format!("def {func}({a}, {b}):\n    return {a} {op} {b}\n");
            for fm in &collect_file_mutations(&source) {
                let _ = generate_trampoline(fm, "test_mod");
            }
        }

        // Target 3, P2: wrapper_code contains `def <original_name>(`.
        #[test]
        fn prop_generate_trampoline_wrapper_has_def(
            func in prop_oneof![Just("foo"), Just("bar"), Just("compute"), Just("check_val")],
            a    in prop_oneof![Just("a"),   Just("x"),   Just("lhs")],
            b    in prop_oneof![Just("b"),   Just("y"),   Just("rhs")],
            op   in prop_oneof![Just("+"),   Just("-"),   Just("*"),   Just("//")],
        ) {
            let source = format!("def {func}({a}, {b}):\n    return {a} {op} {b}\n");
            for fm in &collect_file_mutations(&source) {
                let output = generate_trampoline(fm, "test_mod");
                prop_assert!(
                    output.wrapper_code.contains(&format!("def {}(", fm.name)),
                    "wrapper must contain 'def {}(': {}", fm.name, output.wrapper_code
                );
            }
        }

        // Target 3, P3: module_code contains the mangled orig and at least one variant.
        #[test]
        fn prop_generate_trampoline_module_has_orig_and_variant(
            func in prop_oneof![Just("foo"), Just("bar"), Just("compute")],
            a    in prop_oneof![Just("a"),   Just("x")],
            b    in prop_oneof![Just("b"),   Just("y")],
            op   in prop_oneof![Just("+"),   Just("-"),   Just("*"),   Just("//")],
        ) {
            let source = format!("def {func}({a}, {b}):\n    return {a} {op} {b}\n");
            for fm in &collect_file_mutations(&source) {
                let mangled = mangle_name(&fm.name, fm.class_name.as_deref());
                let output = generate_trampoline(fm, "test_mod");
                prop_assert!(
                    output.module_code.contains(&format!("{mangled}__irradiate_orig")),
                    "module_code must contain orig: {}", output.module_code
                );
                prop_assert!(
                    output.module_code.contains(&format!("{mangled}__irradiate_1")),
                    "module_code must contain variant _1: {}", output.module_code
                );
            }
        }

        // Target 3, P4: mutant_keys are well-formed "module.mangled__irradiate_N".
        #[test]
        fn prop_generate_trampoline_keys_wellformed(
            func in prop_oneof![Just("foo"), Just("bar"), Just("compute")],
            a    in prop_oneof![Just("a"),   Just("x")],
            b    in prop_oneof![Just("b"),   Just("y")],
            op   in prop_oneof![Just("+"),   Just("-"),   Just("*"),   Just("//")],
        ) {
            let source = format!("def {func}({a}, {b}):\n    return {a} {op} {b}\n");
            for fm in &collect_file_mutations(&source) {
                let output = generate_trampoline(fm, "test_mod");
                for key in &output.mutant_keys {
                    prop_assert!(
                        key.starts_with("test_mod."),
                        "key must be module-qualified: {key}"
                    );
                    prop_assert!(
                        key.contains("__irradiate_"),
                        "key must contain __irradiate_: {key}"
                    );
                    let num = key.rsplit("__irradiate_").next().unwrap_or("");
                    prop_assert!(
                        !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()),
                        "variant number must be all digits: {key}"
                    );
                }
            }
        }
    }
}
