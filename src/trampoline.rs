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

/// Generate the full trampolined output for a single function.
///
/// Returns (generated_code, list_of_mutant_keys) where mutant_keys are
/// like "module.x_func__mutmut_1".
pub fn generate_trampoline(fm: &FunctionMutations, module_name: &str) -> (String, Vec<String>) {
    let mangled = mangle_name(&fm.name, fm.class_name.as_deref());
    let mut lines = Vec::new();
    let mut mutant_keys = Vec::new();

    // Original function, renamed
    let orig_name = format!("{mangled}__mutmut_orig");
    let renamed_orig = rename_function(&fm.source, &fm.name, &orig_name);
    lines.push(renamed_orig);
    lines.push(String::new());

    // Mutant variants
    for (i, mutation) in fm.mutations.iter().enumerate() {
        let variant_name = format!("{mangled}__mutmut_{}", i + 1);
        let mutated_source = apply_mutation(&fm.source, mutation);
        let renamed_variant = rename_function(&mutated_source, &fm.name, &variant_name);
        lines.push(renamed_variant);
        lines.push(String::new());

        mutant_keys.push(format!("{module_name}.{variant_name}"));
    }

    // Lookup dict
    lines.push(format!("{mangled}__mutmut_mutants = {{"));
    for (i, _) in fm.mutations.iter().enumerate() {
        let variant_name = format!("{mangled}__mutmut_{}", i + 1);
        lines.push(format!("    '{variant_name}': {variant_name},"));
    }
    lines.push("}".to_string());

    // Set __name__ on orig for trampoline dispatch
    lines.push(format!("{orig_name}.__name__ = '{mangled}'"));
    lines.push(String::new());

    // Trampoline wrapper with original name and signature
    let self_arg = if fm.class_name.is_some() {
        // For class methods, pass self explicitly
        "self"
    } else {
        "None"
    };

    // Build the call args, stripping 'self' for class methods
    let params_text = &fm.params_source;
    let wrapper = generate_wrapper_function(&fm.name, &mangled, params_text, self_arg, fm.is_async);
    lines.push(wrapper);

    (lines.join("\n"), mutant_keys)
}

fn generate_wrapper_function(
    original_name: &str,
    mangled_name: &str,
    params_source: &str,
    self_arg: &str,
    is_async: bool,
) -> String {
    let async_prefix = if is_async { "async " } else { "" };
    let await_prefix = if is_async { "await " } else { "" };

    // Parse parameter names from the params source to build forwarding args.
    // We need to collect positional args into a tuple and kwargs into a dict.
    let (pos_args, kw_args) = parse_param_names(params_source, self_arg != "None");

    let args_list = if pos_args.is_empty() {
        "()".to_string()
    } else {
        format!(
            "({}{})",
            pos_args.join(", "),
            if pos_args.len() == 1 { "," } else { "" }
        )
    };

    let kwargs_dict = if kw_args.is_empty() {
        "{}".to_string()
    } else {
        let entries: Vec<String> = kw_args.iter().map(|k| format!("'{k}': {k}")).collect();
        format!("{{{}}}", entries.join(", "))
    };

    format!(
        "{async_prefix}def {original_name}({params_source}):\n    \
         return {await_prefix}_irradiate_trampoline({mangled_name}__mutmut_orig, {mangled_name}__mutmut_mutants, {args_list}, {kwargs_dict}, {self_arg})",
    )
}

/// Parse parameter names from a params source string.
/// Returns (positional_args, keyword_only_args).
fn parse_param_names(params_source: &str, has_self: bool) -> (Vec<String>, Vec<String>) {
    let mut pos_args = Vec::new();
    let mut kw_args = Vec::new();
    let mut after_star = false;

    for part in params_source.split(',') {
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

        // Handle *args
        if part.starts_with("**") {
            // **kwargs — skip, handled separately
            continue;
        }
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

    (pos_args, kw_args)
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
        return orig(*call_args, **call_kwargs) if call_args is not None else None
    if active == 'fail':
        raise _ih.ProgrammaticFailException()
    if active == 'stats':
        _ih.record_hit(orig.__module__ + '.' + orig.__name__)
        return orig(*call_args, **call_kwargs) if call_args is not None else None
    prefix = orig.__module__ + '.' + orig.__name__ + '__mutmut_'
    if not active.startswith(prefix):
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

        let (code, keys) = generate_trampoline(&fms[0], "my_lib");
        assert!(
            code.contains("x_add__mutmut_orig"),
            "Should have renamed original"
        );
        assert!(
            code.contains("x_add__mutmut_1"),
            "Should have at least one variant"
        );
        assert!(
            code.contains("x_add__mutmut_mutants"),
            "Should have lookup dict"
        );
        assert!(code.contains("def add("), "Should have trampoline wrapper");
        assert!(!keys.is_empty(), "Should produce mutant keys");
        assert!(
            keys[0].starts_with("my_lib.x_add__mutmut_"),
            "Keys should be module-qualified"
        );
    }
}
