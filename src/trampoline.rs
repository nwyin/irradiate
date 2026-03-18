//! Trampoline code generation: takes function mutations and produces
//! the trampolined output (orig + variants + lookup dict + wrapper).

use crate::mutation::{apply_mutation, FunctionMutations};

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
    /// Mutant keys like "module.x_func__mutmut_1".
    pub mutant_keys: Vec<String>,
}

/// Generate the full trampolined output for a single function.
pub fn generate_trampoline(fm: &FunctionMutations, module_name: &str) -> TrampolineOutput {
    let mangled = mangle_name(&fm.name, fm.class_name.as_deref());
    let mut module_lines = Vec::new();
    let mut mutant_keys = Vec::new();

    // Original function, renamed
    let orig_name = format!("{mangled}__mutmut_orig");
    let renamed_orig = rename_function(&fm.source, &fm.name, &orig_name);
    module_lines.push(renamed_orig);
    module_lines.push(String::new());

    // Mutant variants
    for (i, mutation) in fm.mutations.iter().enumerate() {
        let variant_name = format!("{mangled}__mutmut_{}", i + 1);
        let mutated_source = apply_mutation(&fm.source, mutation);
        let renamed_variant = rename_function(&mutated_source, &fm.name, &variant_name);
        module_lines.push(renamed_variant);
        module_lines.push(String::new());

        mutant_keys.push(format!("{module_name}.{variant_name}"));
    }

    // Lookup dict
    module_lines.push(format!("{mangled}__mutmut_mutants = {{"));
    for (i, _) in fm.mutations.iter().enumerate() {
        let variant_name = format!("{mangled}__mutmut_{}", i + 1);
        module_lines.push(format!("    '{variant_name}': {variant_name},"));
    }
    module_lines.push("}".to_string());

    // Set __name__ on orig for trampoline dispatch
    module_lines.push(format!("{orig_name}.__name__ = '{mangled}'"));

    // Trampoline wrapper with original name and signature
    let self_arg = if fm.class_name.is_some() { "self" } else { "None" };
    let params_text = &fm.params_source;
    let wrapper_code =
        generate_wrapper_function(&fm.name, &mangled, params_text, self_arg, fm.is_async, fm.is_generator, &fm.return_annotation);

    TrampolineOutput {
        module_code: module_lines.join("\n"),
        wrapper_code,
        mutant_keys,
    }
}

fn generate_wrapper_function(
    original_name: &str,
    mangled_name: &str,
    params_source: &str,
    self_arg: &str,
    is_async: bool,
    is_generator: bool,
    return_annotation: &str,
) -> String {
    let async_prefix = if is_async { "async " } else { "" };

    // Parse parameter names from the params source to build forwarding args.
    // We need to collect positional args into a tuple and kwargs into a dict.
    let (pos_args, kw_args, kwargs_name) = parse_param_names(params_source, self_arg != "None");

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

    let trampoline_call = format!(
        "_irradiate_trampoline({mangled_name}__mutmut_orig, {mangled_name}__mutmut_mutants, {args_list}, {kwargs_dict}, {self_arg})"
    );

    // Choose the correct dispatch based on function kind:
    //   async generator  → async for _item in trampoline(...): yield _item
    //   sync generator   → yield from trampoline(...)
    //   async regular    → return await trampoline(...)
    //   sync regular     → return trampoline(...)
    let body = match (is_async, is_generator) {
        (true, true) => format!("    async for _item in {trampoline_call}:\n        yield _item"),
        (false, true) => format!("    yield from {trampoline_call}"),
        (true, false) => format!("    return await {trampoline_call}"),
        (false, false) => format!("    return {trampoline_call}"),
    };

    format!("{async_prefix}def {original_name}({params_source}){return_annotation}:\n{body}")
}

/// Strip inline comments from a params source string (line by line).
/// This handles `# type: ignore[override]` and similar annotations.
fn strip_inline_comments(s: &str) -> String {
    s.lines()
        .map(|line| {
            // Strip everything after the first '#' on each line.
            // Per task spec, we don't need to handle '#' inside string literals.
            if let Some(pos) = line.find('#') {
                &line[..pos]
            } else {
                line
            }
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

/// Parse parameter names from a params source string.
/// Returns (positional_args, keyword_only_args, kwargs_name).
fn parse_param_names(params_source: &str, has_self: bool) -> (Vec<String>, Vec<String>, Option<String>) {
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

    // Strip 'self' for class methods
    if has_self && !pos_args.is_empty() && pos_args[0] == "self" {
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
    prefix = orig.__module__ + '.' + orig.__name__ + '__mutmut_'
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
        let renamed = rename_function(source, "add", "x_add__mutmut_orig");
        assert!(renamed.starts_with("def x_add__mutmut_orig("));
        assert!(renamed.contains("return a + b"));
    }

    #[test]
    fn test_generate_trampoline() {
        let source = "def add(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());

        let output = generate_trampoline(&fms[0], "my_lib");
        assert!(
            output.module_code.contains("x_add__mutmut_orig"),
            "Should have renamed original"
        );
        assert!(
            output.module_code.contains("x_add__mutmut_1"),
            "Should have at least one variant"
        );
        assert!(
            output.module_code.contains("x_add__mutmut_mutants"),
            "Should have lookup dict"
        );
        assert!(output.wrapper_code.contains("def add("), "Should have trampoline wrapper");
        assert!(!output.mutant_keys.is_empty(), "Should produce mutant keys");
        assert!(
            output.mutant_keys[0].starts_with("my_lib.x_add__mutmut_"),
            "Keys should be module-qualified"
        );
    }

    // INV-1: Parameters with generic type annotations parse to the correct name only.
    #[test]
    fn test_parse_param_names_generic_annotation() {
        let (pos_args, kw_args, kwargs) = parse_param_names("self, mapping: cabc.Mapping[str, t.Any], /", true);
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
        let (pos_args, kw_args, kwargs) = parse_param_names("self, x: Dict[str, List[int]], y: int", true);
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
        let (pos_args, kw_args, kwargs) = parse_param_names("self, x: Tuple[int, ...], *, key: str", true);
        assert_eq!(pos_args, vec!["x"]);
        assert_eq!(kw_args, vec!["key"]);
        assert_eq!(kwargs, None);
    }

    // Multiple bracket types: Dict[str, Tuple[int, ...]].
    #[test]
    fn test_parse_param_names_deeply_nested() {
        let (pos_args, kw_args, kwargs) = parse_param_names("self, x: Dict[str, Tuple[int, ...]], y: int", true);
        assert_eq!(pos_args, vec!["x", "y"]);
        assert_eq!(kw_args, Vec::<String>::new());
        assert_eq!(kwargs, None);
    }

    // Positional-only separator after bracketed annotation.
    #[test]
    fn test_parse_param_names_pos_only_after_bracket() {
        let (pos_args, kw_args, kwargs) = parse_param_names("self, mapping: Mapping[str, Any], /", true);
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
        let wrapper = generate_wrapper_function("func_with_star", "x_func_with_star", "a, /, b, *, c, **kwargs", "None", false, false, "");
        // kwargs must be spread into the call_kwargs dict
        assert!(wrapper.contains("**kwargs"), "wrapper must forward **kwargs: {wrapper}");
        assert!(wrapper.contains("'c': c"), "wrapper must include c in call_kwargs: {wrapper}");
    }

    // INV-1: Return type annotation is included in wrapper def line.
    #[test]
    fn test_wrapper_return_annotation_preserved() {
        let wrapper = generate_wrapper_function("some_func", "x_some_func", "a, b: str = \"111\"", "None", false, false, " -> int | None");
        assert!(wrapper.starts_with("def some_func(a, b: str = \"111\") -> int | None:"), "wrapper must include return annotation: {wrapper}");
    }

    // INV-3: Wrapper without return annotation or kwargs still correct.
    #[test]
    fn test_wrapper_no_annotation_no_kwargs() {
        let wrapper = generate_wrapper_function("add", "x_add", "a, b", "None", false, false, "");
        assert!(wrapper.starts_with("def add(a, b):"), "wrapper def line must be clean: {wrapper}");
    }

    // INV: generate_trampoline produces a wrapper with return annotation from function source.
    #[test]
    fn test_generate_trampoline_return_annotation() {
        let source = "def some_func(a, b: str = \"111\", c: int = 0) -> int | None:\n    return a + c\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should find mutations in some_func");
        let output = generate_trampoline(&fms[0], "my_lib");
        assert!(
            output.wrapper_code.contains("-> int | None"),
            "trampoline wrapper must preserve return annotation: {}",
            output.wrapper_code
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
        let source = "class Point:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n";
        let fms = collect_file_mutations(source);
        let class_fm = fms.iter().find(|fm| fm.class_name.is_some()).expect("should find class method");
        let output = generate_trampoline(class_fm, "point_module");
        assert!(
            output.wrapper_code.contains(", self)"),
            "Class method wrapper should pass self as self_arg; got: {}",
            output.wrapper_code
        );
    }

    // INV-2: Generator wrapper uses `yield from` instead of `return`.
    // Uses `if n > 0: yield n` to guarantee a compop mutation (so the function is collected).
    #[test]
    fn test_generator_wrapper_uses_yield_from() {
        let source = "def gen(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "generator with compop must produce mutations");
        let output = generate_trampoline(&fms[0], "gen_mod");
        assert!(
            output.wrapper_code.contains("yield from _irradiate_trampoline("),
            "Generator wrapper must use 'yield from', got:\n{}",
            output.wrapper_code
        );
        assert!(
            !output.wrapper_code.contains("return "),
            "Generator wrapper must NOT use 'return', got:\n{}",
            output.wrapper_code
        );
    }

    // INV-3: Async generator wrapper uses `async for ... yield` instead of `return await`.
    #[test]
    fn test_async_generator_wrapper_uses_async_for_yield() {
        let source = "async def agen(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "async generator with compop must produce mutations");
        let output = generate_trampoline(&fms[0], "agen_mod");
        assert!(
            output.wrapper_code.contains("async for _item in _irradiate_trampoline("),
            "Async generator wrapper must use 'async for _item in', got:\n{}",
            output.wrapper_code
        );
        assert!(
            output.wrapper_code.contains("yield _item"),
            "Async generator wrapper must yield _item, got:\n{}",
            output.wrapper_code
        );
        assert!(
            !output.wrapper_code.contains("return "),
            "Async generator wrapper must NOT use 'return', got:\n{}",
            output.wrapper_code
        );
        assert!(
            output.wrapper_code.starts_with("async def "),
            "Async generator wrapper must be an async def, got:\n{}",
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
            output.wrapper_code.contains("return await _irradiate_trampoline("),
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
            output.wrapper_code.contains("return _irradiate_trampoline("),
            "Sync regular wrapper must use 'return', got:\n{}",
            output.wrapper_code
        );
        assert!(
            !output.wrapper_code.contains("yield"),
            "Sync regular wrapper must NOT use 'yield', got:\n{}",
            output.wrapper_code
        );
    }
}
