//! Tree-sitter-backed mutation collector.
//!
//! Alternative to the libcst collector in `mutation.rs`. Uses tree-sitter for parsing so that
//! byte spans come directly from the parser — no monotonic cursor hack needed.
//!
//! Safety checks (matching the libcst collector):
//! - Enum subclasses are skipped entirely (EnumMeta treats all class-body names as candidates).
//! - Functions with `nonlocal` anywhere in their subtree are skipped (scope chain breaks on extract).
//! - `len` and `isinstance` calls are not arg_removal-mutated (they're trivially killed / noisy).

use crate::mutation::{FunctionMutations, Mutation};
use tree_sitter::{Node, Parser, Tree};

const NEVER_MUTATE_FUNCTIONS: &[&str] = &["__getattribute__", "__setattr__", "__new__"];

/// Enum base classes whose methods must not be mutated.
///
/// Python's `EnumMeta` metaclass processes ALL names in the class body and treats
/// non-descriptor, non-dunder names as enum member candidates. Trampoline artifacts
/// (mangled method defs, mutants dicts, `__name__` assignments) placed inside an
/// Enum class body are misinterpreted as member definitions, causing `TypeError`
/// at class creation time (e.g. `int.__new__(cls, dict(...))` for IntEnum).
const ENUM_BASES: &[&str] = &[
    "Enum",
    "IntEnum",
    "StrEnum",
    "Flag",
    "IntFlag",
    "enum.Enum",
    "enum.IntEnum",
    "enum.StrEnum",
    "enum.Flag",
    "enum.IntFlag",
];

/// Builtin function calls that are never arg_removal-mutated.
static NEVER_MUTATE_FUNCTION_CALLS: &[&str] = &["len", "isinstance"];

const BINOP_SWAPS: &[(&str, &str)] = &[
    ("+", "-"),
    ("-", "+"),
    ("*", "/"),
    ("/", "*"),
    ("//", "/"),
    ("%", "/"),
    ("**", "*"),
    ("<<", ">>"),
    (">>", "<<"),
    ("&", "|"),
    ("|", "&"),
    ("^", "&"),
];
const BOOLOP_SWAPS: &[(&str, &str)] = &[("and", "or"), ("or", "and")];
const COMPOP_SWAPS: &[(&str, &str)] = &[
    ("<=", "<"),
    (">=", ">"),
    ("<", "<="),
    (">", ">="),
    ("==", "!="),
    ("!=", "=="),
    ("is not", "is"),
    ("is", "is not"),
    ("not in", "in"),
    ("in", "not in"),
];
const AUGOP_SWAPS: &[(&str, &str)] = &[
    ("+=", "-="),
    ("-=", "+="),
    ("*=", "/="),
    ("/=", "*="),
    ("//=", "/="),
    ("%=", "/="),
    ("**=", "*="),
    ("<<=", ">>="),
    (">>=", "<<="),
    ("&=", "|="),
    ("|=", "&="),
    ("^=", "&="),
];
const METHOD_SWAPS: &[(&str, &str)] = &[
    ("lower", "upper"),
    ("upper", "lower"),
    ("lstrip", "rstrip"),
    ("rstrip", "lstrip"),
    ("find", "rfind"),
    ("rfind", "find"),
    ("ljust", "rjust"),
    ("rjust", "ljust"),
    ("index", "rindex"),
    ("rindex", "index"),
    ("removeprefix", "removesuffix"),
    ("removesuffix", "removeprefix"),
    ("partition", "rpartition"),
    ("rpartition", "partition"),
];
const CONDITIONAL_METHOD_SWAPS: &[(&str, &str)] = &[("split", "rsplit"), ("rsplit", "split")];

pub fn collect_file_mutations_tree_sitter(source: &str) -> Vec<FunctionMutations> {
    let tree = match parse_python(source) {
        Some(tree) => tree,
        None => return vec![],
    };

    let ignored_lines = pragma_no_mutate_lines(source);
    let root = tree.root_node();
    let mut results = Vec::new();
    let mut cursor = root.walk();

    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                if let Some(fm) = collect_function_mutations(child, None, source, &ignored_lines) {
                    results.push(fm);
                }
            }
            "class_definition" => {
                collect_class_methods(child, source, &ignored_lines, &mut results);
            }
            "decorated_definition" => {
                // Match the libcst collector: skip decorated definitions entirely.
            }
            _ => {}
        }
    }

    results
}

fn parse_python(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_python::LANGUAGE.into()).ok()?;
    let tree = parser.parse(source, None)?;
    if tree.root_node().has_error() {
        return None;
    }
    Some(tree)
}

fn collect_class_methods(
    class_node: Node<'_>,
    source: &str,
    ignored_lines: &std::collections::HashSet<usize>,
    results: &mut Vec<FunctionMutations>,
) {
    let class_name = match class_node.child_by_field_name("name") {
        Some(name) => node_text(source, name).to_string(),
        None => return,
    };

    // Skip Enum subclasses entirely — EnumMeta treats all class-body names as member candidates,
    // so trampoline artifacts (mangled defs, mutants dicts) cause TypeError at class creation time.
    if is_enum_subclass(class_node, source) {
        return;
    }

    let body = match class_node.child_by_field_name("body") {
        Some(body) => body,
        None => return,
    };

    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "function_definition" {
            if let Some(fm) =
                collect_function_mutations(child, Some(class_name.as_str()), source, ignored_lines)
            {
                results.push(fm);
            }
        }
    }
}

/// Returns true if the class_definition node inherits from a known Enum base.
fn is_enum_subclass(class_node: Node<'_>, source: &str) -> bool {
    // tree-sitter-python: class_definition has a "superclasses" field which is an argument_list.
    let superclasses = match class_node.child_by_field_name("superclasses") {
        Some(sc) => sc,
        None => return false,
    };
    let mut cursor = superclasses.walk();
    for base in superclasses.named_children(&mut cursor) {
        let base_text = node_text(source, base).trim();
        if ENUM_BASES.contains(&base_text) {
            return true;
        }
    }
    false
}

fn collect_function_mutations(
    function_node: Node<'_>,
    class_name: Option<&str>,
    source: &str,
    ignored_lines: &std::collections::HashSet<usize>,
) -> Option<FunctionMutations> {
    let name_node = function_node.child_by_field_name("name")?;
    let name = node_text(source, name_node).to_string();
    if NEVER_MUTATE_FUNCTIONS.contains(&name.as_str()) {
        return None;
    }

    let body = function_node.child_by_field_name("body")?;

    // Skip functions whose body contains any `nonlocal` statement at any depth.
    // The trampoline renames/extracts functions to module scope, breaking nonlocal scope chains —
    // Python raises SyntaxError at import time when the renamed variant is encountered.
    if subtree_contains_kind(body, "nonlocal_statement") {
        return None;
    }

    let fn_start = function_node.start_byte();
    let fn_end = function_node.end_byte();
    let func_source = &source[fn_start..fn_end];
    let params_source = function_node
        .child_by_field_name("parameters")
        .map(|node| {
            let text = node_text(source, node);
            // tree-sitter `parameters` node includes outer `(` and `)`, but the trampoline
            // generator wraps params in its own parens in `def name({params}):`. Strip them.
            if text.starts_with('(') && text.ends_with(')') {
                text[1..text.len() - 1].to_string()
            } else {
                text.to_string()
            }
        })
        .unwrap_or_default();
    let return_annotation = function_node
        .child_by_field_name("return_type")
        .map(|node| format!(" -> {}", node_text(source, node)))
        .unwrap_or_default();
    let is_async = source[fn_start..name_node.start_byte()].contains("async");
    // is_generator must NOT cross nested function_definition boundaries: only the function's own
    // scope level matters. Use body_contains_yield_at_scope instead of subtree_contains_kind.
    let is_generator = body_contains_yield_at_scope(body);

    let mut mutations = Vec::new();
    collect_default_arg_mutations(function_node, source, fn_start, &mut mutations);
    let mut walk = body.walk();
    for child in body.named_children(&mut walk) {
        collect_node_mutations(child, source, fn_start, function_node.id(), &mut mutations);
    }

    filter_ignored_lines(source, fn_start, ignored_lines, &mut mutations);

    if mutations.is_empty() {
        return None;
    }

    // tree-sitter positions are 0-indexed; convert to 1-indexed line numbers.
    let start_line = function_node.start_position().row + 1;
    let end_line = function_node.end_position().row + 1;

    Some(FunctionMutations {
        name,
        class_name: class_name.map(ToOwned::to_owned),
        source: func_source.to_string(),
        params_source,
        return_annotation,
        is_async,
        is_generator,
        mutations,
        start_line,
        end_line,
        byte_offset: fn_start,
    })
}

fn collect_node_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    owner_function_id: usize,
    mutations: &mut Vec<Mutation>,
) {
    match node.kind() {
        "function_definition" if node.id() != owner_function_id => return,
        "class_definition" => return,
        "binary_operator" => add_binary_operator_mutation(node, source, fn_start, mutations),
        "boolean_operator" => add_boolean_operator_mutation(node, source, fn_start, mutations),
        "comparison_operator" => add_comparison_mutation(node, source, fn_start, mutations),
        "not_operator" => add_not_operator_mutation(node, source, fn_start, mutations),
        "unary_operator" => add_unary_operator_mutation(node, source, fn_start, mutations),
        "identifier" => add_identifier_mutation(node, source, fn_start, mutations),
        "true" | "false" => add_literal_name_mutation(node, source, fn_start, mutations),
        "integer" => add_integer_mutation(node, source, fn_start, mutations),
        "float" => add_float_mutation(node, source, fn_start, mutations),
        "string" => add_string_mutation(node, source, fn_start, mutations),
        "return_statement" => add_return_statement_mutations(node, source, fn_start, mutations),
        "expression_statement" => {
            add_expression_statement_mutation(node, source, fn_start, mutations);
        }
        "assignment" => add_assignment_mutations(node, source, fn_start, mutations),
        "augmented_assignment" => add_augmented_assignment_mutations(node, source, fn_start, mutations),
        "if_statement" | "elif_clause" | "while_statement" => {
            add_condition_negation_statement(node, source, fn_start, mutations);
            add_loop_mutation(node, source, fn_start, mutations);
        }
        "assert_statement" => add_condition_negation_assert(node, source, fn_start, mutations),
        "for_statement" => add_loop_mutation(node, source, fn_start, mutations),
        "break_statement" => add_keyword_swap(node, "continue", source, fn_start, mutations),
        "continue_statement" => add_keyword_swap(node, "break", source, fn_start, mutations),
        "except_clause" => add_exception_type_mutation(node, source, fn_start, mutations),
        "call" => {
            add_arg_removal_mutations(node, source, fn_start, mutations);
            add_method_mutations(node, source, fn_start, mutations);
            add_dict_kwarg_mutations(node, source, fn_start, mutations);
        }
        "match_statement" => add_match_case_removal_mutations(node, source, fn_start, mutations),
        "raise_statement" => add_raise_statement_mutation(node, source, fn_start, mutations),
        "lambda" => add_lambda_mutation(node, source, fn_start, mutations),
        "conditional_expression" => {
            add_ternary_swap_mutation(node, source, fn_start, mutations);
            add_condition_negation_ternary(node, source, fn_start, mutations);
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_node_mutations(child, source, fn_start, owner_function_id, mutations);
    }
}

fn add_binary_operator_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(op_node) = find_operator_child(node, BINOP_SWAPS.iter().map(|(from, _)| *from)) else {
        return;
    };
    let op_text = node_text(source, op_node);
    let Some((_, replacement)) = BINOP_SWAPS.iter().find(|(from, _)| *from == op_text) else {
        return;
    };
    record_mutation(
        op_text,
        replacement,
        "binop_swap",
        op_node.start_byte() - fn_start,
        mutations,
    );
}

fn add_boolean_operator_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(op_node) = find_operator_child(node, BOOLOP_SWAPS.iter().map(|(from, _)| *from))
    else {
        return;
    };
    let op_text = node_text(source, op_node);
    let Some((_, replacement)) = BOOLOP_SWAPS.iter().find(|(from, _)| *from == op_text) else {
        return;
    };
    record_mutation(
        op_text,
        replacement,
        "boolop_swap",
        op_node.start_byte() - fn_start,
        mutations,
    );
}

fn add_comparison_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(op_node) = find_operator_child(node, COMPOP_SWAPS.iter().map(|(from, _)| *from))
    else {
        return;
    };
    let op_text = node_text(source, op_node);
    let Some((_, replacement)) = COMPOP_SWAPS.iter().find(|(from, _)| *from == op_text) else {
        return;
    };
    record_mutation(
        op_text,
        replacement,
        "compop_swap",
        op_node.start_byte() - fn_start,
        mutations,
    );
}

fn add_not_operator_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(argument) = node.child_by_field_name("argument") else {
        return;
    };
    record_mutation(
        node_text(source, node),
        node_text(source, argument),
        "unary_removal",
        node.start_byte() - fn_start,
        mutations,
    );
}

fn add_unary_operator_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(argument) = node.child_by_field_name("argument") else {
        return;
    };
    let Some(operator) = node.child_by_field_name("operator") else {
        return;
    };
    let operator_text = node_text(source, operator);
    match operator_text {
        "~" => record_mutation(
            node_text(source, node),
            node_text(source, argument),
            "unary_removal",
            node.start_byte() - fn_start,
            mutations,
        ),
        "+" => {
            let replacement = format!("-{}", node_text(source, argument));
            record_mutation(
                node_text(source, node),
                &replacement,
                "unary_swap",
                node.start_byte() - fn_start,
                mutations,
            );
        }
        "-" => {
            let replacement = format!("+{}", node_text(source, argument));
            record_mutation(
                node_text(source, node),
                &replacement,
                "unary_swap",
                node.start_byte() - fn_start,
                mutations,
            );
        }
        _ => {}
    }
}

fn add_identifier_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let text = node_text(source, node);
    if text == "deepcopy" {
        record_mutation(text, "copy", "name_swap", node.start_byte() - fn_start, mutations);
    }
}

fn add_literal_name_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let text = node_text(source, node);
    let replacement = match text {
        "True" => "False",
        "False" => "True",
        _ => return,
    };
    record_mutation(text, replacement, "name_swap", node.start_byte() - fn_start, mutations);
}

fn add_integer_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let text = node_text(source, node);
    if let Ok(value) = text.replace('_', "").parse::<i64>() {
        let replacement = (value + 1).to_string();
        if replacement != text {
            record_mutation(
                text,
                &replacement,
                "number_mutation",
                node.start_byte() - fn_start,
                mutations,
            );
        }
    }
}

fn add_float_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let text = node_text(source, node);
    if let Ok(value) = text.parse::<f64>() {
        let replacement = format!("{}", value + 1.0);
        if replacement != text {
            record_mutation(
                text,
                &replacement,
                "number_mutation",
                node.start_byte() - fn_start,
                mutations,
            );
        }
    }
}

fn add_string_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let text = node_text(source, node);
    if text.contains("\"\"\"") || text.contains("'''") {
        return;
    }
    let Some(prefix_end) = text.find(['"', '\'']) else {
        return;
    };
    let quote = text.as_bytes()[prefix_end] as char;
    if text.len() < prefix_end + 2 {
        return;
    }
    let prefix = &text[..prefix_end];
    let inner = &text[prefix_end + 1..text.len().saturating_sub(1)];
    if inner.contains(quote) {
        return;
    }

    let replacement = format!("{prefix}{quote}XX{inner}XX{quote}");
    if replacement != text {
        record_mutation(
            text,
            &replacement,
            "string_mutation",
            node.start_byte() - fn_start,
            mutations,
        );
    }

    if !inner.is_empty() {
        let empty = format!("{prefix}{quote}{quote}");
        record_mutation(
            text,
            &empty,
            "string_emptying",
            node.start_byte() - fn_start,
            mutations,
        );
    }
}

fn add_return_statement_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(value_node) = first_named_child(node) else {
        return;
    };
    let value_text = node_text(source, value_node);
    let replacement = if value_text.trim() == "None" {
        "\"\""
    } else {
        "None"
    };
    record_mutation(
        value_text,
        replacement,
        "return_value",
        value_node.start_byte() - fn_start,
        mutations,
    );

    let stmt_text = node_text(source, node);
    if value_text.trim() != "None" {
        record_mutation(
            stmt_text,
            "return None",
            "statement_deletion",
            node.start_byte() - fn_start,
            mutations,
        );
    }
}

fn add_expression_statement_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(value_node) = first_named_child(node) else {
        return;
    };
    let stmt_text = node_text(source, node);
    match value_node.kind() {
        // Docstrings: string-literal expression statements are never deleted.
        "string" => {}
        // Augmented assignment (`x += 1`) has its own handler via `add_augmented_assignment_mutations`.
        // tree-sitter-python wraps it in expression_statement — skip here so we don't add
        // a spurious statement_deletion for augmented assignments.
        "augmented_assignment" => {}
        // Regular assignment (`x = expr`) — statement_deletion is handled here (at expression_statement
        // level) so we get the full statement text including all chained targets (e.g. `a = b = c`).
        // The add_assignment_mutations handler only adds the value-replacement mutation.
        "assignment" => {
            if !stmt_text.trim_start().starts_with("self.") {
                record_mutation(
                    stmt_text,
                    "pass",
                    "statement_deletion",
                    node.start_byte() - fn_start,
                    mutations,
                );
            }
        }
        // All other expression statements (calls, etc.): delete to `pass`.
        _ => {
            record_mutation(
                stmt_text,
                "pass",
                "statement_deletion",
                node.start_byte() - fn_start,
                mutations,
            );
        }
    }
}

fn add_assignment_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(value_node) = last_named_child(node) else {
        return;
    };

    // Chained assignments like `a = b = c` appear in tree-sitter as nested `assignment` nodes:
    // outer has right=assignment(b, c). Skip the outer; recursion will reach the inner assignment
    // `b = c` and produce the correct `b = None` mutation, which applied to the full source
    // yields `a = b = None` — matching the libcst collector's single-target-replacement behavior.
    if value_node.kind() == "assignment" {
        return;
    }

    let assignment_text = node_text(source, node);
    let value_text = node_text(source, value_node);
    let replacement_value = if value_text.trim() == "None" { "\"\"" } else { "None" };
    let prefix_len = value_node.start_byte().saturating_sub(node.start_byte());
    if prefix_len <= assignment_text.len() {
        let new_assignment = format!("{}{}", &assignment_text[..prefix_len], replacement_value);
        record_mutation(
            assignment_text,
            &new_assignment,
            "assignment_mutation",
            node.start_byte() - fn_start,
            mutations,
        );
    }
    // Note: statement_deletion is handled by add_expression_statement_mutation at the
    // expression_statement level, so the full statement text (including all chained targets)
    // is used for deletion. We do NOT add statement_deletion here.
}

fn add_augmented_assignment_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    let Some(right) = node.child_by_field_name("right") else {
        return;
    };
    let Some(operator) = node.child_by_field_name("operator") else {
        return;
    };

    let operator_text = node_text(source, operator);
    if let Some((_, replacement)) = AUGOP_SWAPS.iter().find(|(from, _)| *from == operator_text) {
        record_mutation(
            operator_text,
            replacement,
            "augop_swap",
            operator.start_byte() - fn_start,
            mutations,
        );
    }

    let replacement = format!("{} = {}", node_text(source, left), node_text(source, right));
    record_mutation(
        node_text(source, node),
        &replacement,
        "augassign_to_assign",
        node.start_byte() - fn_start,
        mutations,
    );
}

fn add_condition_negation_statement(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(condition) = node.child_by_field_name("condition") else {
        return;
    };
    let condition_text = node_text(source, condition);
    if condition.kind() == "not_operator" {
        return;
    }
    let replacement = format!("not ({condition_text})");
    record_mutation(
        condition_text,
        &replacement,
        "condition_negation",
        condition.start_byte() - fn_start,
        mutations,
    );
}

fn add_condition_negation_assert(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(condition) = first_named_child(node) else {
        return;
    };
    let condition_text = node_text(source, condition);
    if condition.kind() == "not_operator" {
        return;
    }
    let replacement = format!("not ({condition_text})");
    record_mutation(
        condition_text,
        &replacement,
        "condition_negation",
        condition.start_byte() - fn_start,
        mutations,
    );
}

fn add_loop_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    match node.kind() {
        "for_statement" => {
            let Some(right) = node.child_by_field_name("right") else {
                return;
            };
            let text = node_text(source, right);
            if text.trim() != "[]" {
                record_mutation(
                    text,
                    "[]",
                    "loop_mutation",
                    right.start_byte() - fn_start,
                    mutations,
                );
            }
        }
        "while_statement" => {
            let Some(condition) = node.child_by_field_name("condition") else {
                return;
            };
            let text = node_text(source, condition);
            if text.trim() != "False" {
                record_mutation(
                    text,
                    "False",
                    "loop_mutation",
                    condition.start_byte() - fn_start,
                    mutations,
                );
            }
        }
        _ => {}
    }
}

fn add_keyword_swap(
    node: Node<'_>,
    replacement: &str,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    record_mutation(
        node_text(source, node),
        replacement,
        "keyword_swap",
        node.start_byte() - fn_start,
        mutations,
    );
}

fn add_exception_type_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(value) = node.child_by_field_name("value") else {
        return;
    };
    // `except ValueError as e:` → tree-sitter wraps the type in `as_pattern`:
    //   value: (as_pattern (identifier "ValueError") alias: ...)
    // We should only mutate the exception type, not the full `as_pattern` text.
    let type_node = if value.kind() == "as_pattern" {
        let children: Vec<Node<'_>> = {
            let mut c = value.walk();
            value.named_children(&mut c).collect()
        };
        match children.into_iter().next() {
            Some(n) => n,
            None => return,
        }
    } else {
        value
    };
    let text = node_text(source, type_node);
    if text.trim() != "Exception" {
        record_mutation(
            text,
            "Exception",
            "exception_type",
            type_node.start_byte() - fn_start,
            mutations,
        );
    }
}

/// Delete an explicit `raise Exc(...)` statement to `pass`.
/// Bare `raise` (re-raise in except) has no named children — skip those.
fn add_raise_statement_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    if node.named_child_count() == 0 {
        return; // bare `raise` — re-raise, don't delete
    }
    let stmt_text = node_text(source, node);
    record_mutation(stmt_text, "pass", "statement_deletion", node.start_byte() - fn_start, mutations);
}

/// Lambda mutation: replace `lambda params: body` with `lambda params: None` (or `: 0` if body is None).
///
/// Strategy: find the first `:` in the lambda text (params can't contain `:` except in annotations,
/// but lambda params don't have type annotations in Python syntax), then replace the body text after it.
fn add_lambda_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(body_node) = node.child_by_field_name("body") else {
        return;
    };
    let lambda_text = node_text(source, node);
    let body_text = node_text(source, body_node);
    let replacement_body = if body_text.trim() == "None" { "0" } else { "None" };
    // Find `:` separator: first `:` in the lambda text is always the colon before the body.
    let colon_pos = match lambda_text.find(':') {
        Some(p) => p,
        None => return,
    };
    let after_colon = &lambda_text[colon_pos + 1..];
    let ws_len = after_colon.find(|c: char| !c.is_whitespace()).unwrap_or(0);
    let body_start = colon_pos + 1 + ws_len;
    let body_end = body_start + body_text.len();
    if body_end > lambda_text.len() {
        return; // safety guard
    }
    let replacement = format!(
        "{}{}{}",
        &lambda_text[..body_start],
        replacement_body,
        &lambda_text[body_end..]
    );
    record_mutation(lambda_text, &replacement, "lambda_mutation", node.start_byte() - fn_start, mutations);
}

/// Ternary swap: `body if condition else alternative` → `alternative if condition else body`.
/// Skip if body and alternative are identical (equivalent mutant).
///
/// Per hive notes: tree-sitter-python conditional_expression uses positional named children:
///   children[0] = body, children[1] = condition, children[2] = alternative
fn add_ternary_swap_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let children: Vec<Node<'_>> = {
        let mut c = node.walk();
        node.named_children(&mut c).collect()
    };
    if children.len() < 3 {
        return;
    }
    let body_text = node_text(source, children[0]);
    let alternative_text = node_text(source, children[2]);
    if body_text == alternative_text {
        return; // equivalent mutant
    }
    let full_text = node_text(source, node);
    // Reconstruct swapped ternary: alternative if condition else body.
    // Find the positions of `if` and `else` keywords in the original text.
    let body_end = children[0].end_byte() - node.start_byte();
    let condition_start = children[1].start_byte() - node.start_byte();
    let condition_end = children[1].end_byte() - node.start_byte();
    let alt_start = children[2].start_byte() - node.start_byte();
    // Between body end and condition start there are whitespace + `if`; preserve it.
    let between_body_and_cond = &full_text[body_end..condition_start];
    // Between condition end and alt start there are whitespace + `else`; preserve it.
    let between_cond_and_alt = &full_text[condition_end..alt_start];
    let replacement = format!(
        "{}{}{}{}{}",
        alternative_text, between_body_and_cond, node_text(source, children[1]),
        between_cond_and_alt, body_text
    );
    record_mutation(full_text, &replacement, "ternary_swap", node.start_byte() - fn_start, mutations);
}

/// Condition negation for ternary expression: `body if cond else alt` → `body if not (cond) else alt`.
fn add_condition_negation_ternary(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let children: Vec<Node<'_>> = {
        let mut c = node.walk();
        node.named_children(&mut c).collect()
    };
    if children.len() < 3 {
        return;
    }
    let condition = children[1];
    if condition.kind() == "not_operator" {
        return; // already negated
    }
    let condition_text = node_text(source, condition);
    let replacement = format!("not ({condition_text})");
    record_mutation(condition_text, &replacement, "condition_negation", condition.start_byte() - fn_start, mutations);
}

fn add_arg_removal_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(function_node) = node.child_by_field_name("function") else {
        return;
    };
    let Some(args_node) = node.child_by_field_name("arguments") else {
        return;
    };

    let function_text = node_text(source, function_node);

    // Skip builtins where arg_removal mutations are trivially killed and add noise.
    if NEVER_MUTATE_FUNCTION_CALLS.contains(&function_text) {
        return;
    }

    let args = collect_call_arguments(args_node, source);
    if args.is_empty() {
        return;
    }

    // A generator expression as a sole argument uses no parens:
    //   tuple(x for x in items)
    // tree-sitter inlines these as separate children (call + for_in_clause).
    // Replacing/removing parts produces invalid syntax like
    //   tuple(None, for x in items)
    // so skip arg_removal entirely when any arg is a for_in_clause or if_clause
    // (indicating a generator expression).
    if args.iter().any(|a| a.kind == "for_in_clause" || a.kind == "if_clause") {
        return;
    }

    for (i, arg) in args.iter().enumerate() {
        if arg.text.trim_start().starts_with('*') {
            continue;
        }

        if arg.text.trim() != "None" {
            let new_args: Vec<String> = args
                .iter()
                .enumerate()
                .map(|(j, candidate)| {
                    if i == j {
                        keyword_prefix(candidate)
                            .map(|prefix| format!("{prefix}=None"))
                            .unwrap_or_else(|| "None".to_string())
                    } else {
                        candidate.text.to_string()
                    }
                })
                .collect();
            let new_call = format!("{function_text}({})", new_args.join(", "));
            record_mutation(
                node_text(source, node),
                &new_call,
                "arg_removal",
                node.start_byte() - fn_start,
                mutations,
            );
        }

        if args.len() > 1 {
            let new_args: Vec<&str> = args
                .iter()
                .enumerate()
                .filter_map(|(j, candidate)| (i != j).then_some(candidate.text))
                .collect();
            let new_call = format!("{function_text}({})", new_args.join(", "));
            record_mutation(
                node_text(source, node),
                &new_call,
                "arg_removal",
                node.start_byte() - fn_start,
                mutations,
            );
        }
    }
}

fn add_method_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(function_node) = node.child_by_field_name("function") else {
        return;
    };
    if function_node.kind() != "attribute" {
        return;
    }

    let Some(attribute_node) = function_node.child_by_field_name("attribute") else {
        return;
    };
    let method_name = node_text(source, attribute_node);

    if let Some((_, replacement)) = METHOD_SWAPS.iter().find(|(from, _)| *from == method_name) {
        record_mutation(
            method_name,
            replacement,
            "method_swap",
            attribute_node.start_byte() - fn_start,
            mutations,
        );
    }

    if let Some((_, replacement)) = CONDITIONAL_METHOD_SWAPS
        .iter()
        .find(|(from, _)| *from == method_name)
    {
        let Some(arguments) = node.child_by_field_name("arguments") else {
            return;
        };
        let (positional_count, has_maxsplit_kwarg) = inspect_call_arguments(arguments, source);
        if positional_count == 2 || has_maxsplit_kwarg {
            record_mutation(
                method_name,
                replacement,
                "method_swap",
                attribute_node.start_byte() - fn_start,
                mutations,
            );
        }
    }
}

fn add_dict_kwarg_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(function_node) = node.child_by_field_name("function") else {
        return;
    };
    if node_text(source, function_node) != "dict" {
        return;
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };

    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() != "keyword_argument" {
            continue;
        }
        let Some(name) = child.child_by_field_name("name") else {
            continue;
        };
        let kw = node_text(source, name);
        let replacement = format!("{kw}XX");
        record_mutation(
            kw,
            &replacement,
            "dict_kwarg",
            name.start_byte() - fn_start,
            mutations,
        );
    }
}

fn add_match_case_removal_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(body_node) = node.child_by_field_name("body") else {
        return;
    };

    let case_clauses: Vec<Node<'_>> = {
        let mut cursor = body_node.walk();
        body_node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "case_clause")
            .collect()
    };
    if case_clauses.len() <= 1 {
        return;
    }

    let match_text = node_text(source, node);
    let match_start = node.start_byte();

    for (index, case_node) in case_clauses.iter().enumerate() {
        let case_rel_start = case_node.start_byte() - match_start;
        let case_rel_end = case_clauses
            .get(index + 1)
            .map(|next| next.start_byte() - match_start)
            .unwrap_or(match_text.len());
        let replacement = format!(
            "{}{}",
            &match_text[..case_rel_start],
            &match_text[case_rel_end..]
        );
        record_mutation(
            match_text,
            &replacement,
            "match_case_removal",
            match_start - fn_start,
            mutations,
        );
    }
}

fn collect_call_arguments<'a>(args_node: Node<'a>, source: &'a str) -> Vec<CallArg<'a>> {
    let mut args = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.named_children(&mut cursor) {
        // tree-sitter marks comments as "extra" nodes; they can appear anywhere
        // inside an argument list (e.g. `foo(  # type: ignore\n  x, y)`).
        // Treating them as arguments produces invalid mutations like
        // `foo(# type: ignore, None, y)` where the comment swallows the closing paren.
        if child.is_extra() {
            continue;
        }
        let text = node_text(source, child);
        args.push(CallArg {
            kind: child.kind(),
            text,
        });
    }
    args
}

fn inspect_call_arguments(arguments: Node<'_>, source: &str) -> (usize, bool) {
    let mut positional_count = 0;
    let mut has_maxsplit_kwarg = false;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        match child.kind() {
            "keyword_argument" => {
                if let Some(name) = child.child_by_field_name("name") {
                    has_maxsplit_kwarg |= node_text(source, name) == "maxsplit";
                }
            }
            "list_splat" | "dictionary_splat" => {}
            _ => positional_count += 1,
        }
    }
    (positional_count, has_maxsplit_kwarg)
}

fn collect_default_arg_mutations(
    function_node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(parameters) = function_node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = parameters.walk();
    for child in parameters.named_children(&mut cursor) {
        match child.kind() {
            "default_parameter" | "typed_default_parameter" => {
                let Some(value) = child.child_by_field_name("value") else {
                    continue;
                };
                let value_text = node_text(source, value);
                if let Some(replacement) = compute_default_replacement(value_text) {
                    if replacement.as_str() != value_text {
                        record_mutation(
                            value_text,
                            &replacement,
                            "default_arg",
                            value.start_byte() - fn_start,
                            mutations,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn keyword_prefix<'a>(arg: &'a CallArg<'a>) -> Option<&'a str> {
    if arg.kind != "keyword_argument" {
        return None;
    }
    arg.text.split_once('=').map(|(name, _)| name.trim())
}

fn find_operator_child<'a>(
    node: Node<'a>,
    kinds: impl IntoIterator<Item = &'a str>,
) -> Option<Node<'a>> {
    let kinds: std::collections::HashSet<&str> = kinds.into_iter().collect();
    for i in 0..node.child_count() {
        let child = node.child(i)?;
        if !child.is_named() && kinds.contains(child.kind()) {
            return Some(child);
        }
    }
    None
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let child = node.named_children(&mut cursor).next();
    child
}

fn last_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let child = node.named_children(&mut cursor).last();
    child
}

fn subtree_contains_kind(node: Node<'_>, expected: &str) -> bool {
    if node.kind() == expected {
        return true;
    }
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|child| subtree_contains_kind(child, expected));
    found
}

/// Check if `node` (a function body block) contains a `yield` or `yield_from` at its own scope,
/// NOT crossing into nested function_definition or lambda boundaries.
///
/// This matches the libcst collector's behavior: a function is a generator only if IT yields,
/// not if a nested function inside it yields.
fn body_contains_yield_at_scope(node: Node<'_>) -> bool {
    if node.kind() == "yield" || node.kind() == "yield_from" {
        return true;
    }
    // Don't recurse into nested function definitions or lambdas — they have their own scope.
    if node.kind() == "function_definition" || node.kind() == "lambda" {
        return false;
    }
    let children: Vec<Node<'_>> = {
        let mut cursor = node.walk();
        node.children(&mut cursor).collect()
    };
    children.into_iter().any(body_contains_yield_at_scope)
}

fn filter_ignored_lines(
    source: &str,
    fn_start: usize,
    ignored_lines: &std::collections::HashSet<usize>,
    mutations: &mut Vec<Mutation>,
) {
    if ignored_lines.is_empty() {
        return;
    }
    mutations.retain(|mutation| {
        !ignored_lines.contains(&offset_to_line(source, fn_start + mutation.start))
    });
}

fn pragma_no_mutate_lines(source: &str) -> std::collections::HashSet<usize> {
    source
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            if line.contains("# pragma:")
                && line
                    .split("# pragma:")
                    .last()
                    .is_some_and(|suffix| suffix.contains("no mutate"))
            {
                Some(i + 1)
            } else {
                None
            }
        })
        .collect()
}

fn offset_to_line(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

fn node_text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

fn record_mutation(
    original: &str,
    replacement: &str,
    operator: &'static str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    mutations.push(Mutation {
        start,
        end: start + original.len(),
        original: original.to_string(),
        replacement: replacement.to_string(),
        operator,
    });
}

fn compute_default_replacement(default_text: &str) -> Option<String> {
    let trimmed = default_text.trim();

    if trimmed == "None" {
        return Some("\"\"".to_string());
    }
    if trimmed == "True" {
        return Some("False".to_string());
    }
    if trimmed == "False" {
        return Some("True".to_string());
    }

    // Integer: try parsing without underscores (Python allows 1_000 etc.)
    if let Ok(n) = trimmed.replace('_', "").parse::<i64>() {
        let r = (n + 1).to_string();
        if r.as_str() != trimmed {
            return Some(r);
        }
    }

    // Float: only try parsing if it looks like a float (contains `.` or `e`/`E`).
    if trimmed.contains('.') || trimmed.to_lowercase().contains('e') {
        if let Ok(n) = trimmed.parse::<f64>() {
            let r = format!("{}", n + 1.0);
            if r != trimmed {
                return Some(r);
            }
        }
    }

    // String literal: detect quoted form (with optional prefix like r, b, f).
    if let Some(prefix_end) = trimmed.find(['"', '\'']) {
        let quote_char = trimmed.as_bytes()[prefix_end] as char;
        let rest = &trimmed[prefix_end..];
        let triple = if quote_char == '"' { "\"\"\"" } else { "'''" };
        if !rest.starts_with(triple) && trimmed.ends_with(quote_char) && trimmed.len() >= 2 {
            let prefix = &trimmed[..prefix_end];
            let inner = &trimmed[prefix_end + 1..trimmed.len() - 1];
            if !inner.contains(quote_char) {
                if inner.is_empty() {
                    return Some(format!("{prefix}{quote_char}XX{quote_char}"));
                } else {
                    return Some(format!("{prefix}{quote_char}XX{inner}XX{quote_char}"));
                }
            }
        }
    }

    // Fallback: replace with None.
    Some("None".to_string())
}

#[derive(Clone, Copy)]
struct CallArg<'a> {
    kind: &'a str,
    text: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutation::apply_mutation;
    use libcst_native::parse_module;

    // INV-1: every mutation has start < end and start + original.len() == end
    fn check_span_invariant(fm: &FunctionMutations) {
        for m in &fm.mutations {
            assert!(m.start < m.end, "mutation start must be < end: {:?}", m);
            assert_eq!(
                m.start + m.original.len(),
                m.end,
                "mutation end must equal start + original.len(): {:?}",
                m
            );
        }
    }

    #[test]
    fn tree_sitter_collects_multiline_boolop_at_exact_span() {
        let source = concat!(
            "def f(default_map, info_name):\n",
            "    if (\n",
            "        default_map is None\n",
            "        and info_name is not None\n",
            "    ):\n",
            "        return 1\n",
            "    return 0\n",
        );

        let fms = collect_file_mutations_tree_sitter(source);
        let fm = &fms[0];
        check_span_invariant(fm);

        let mutation = fm
            .mutations
            .iter()
            .find(|m| m.operator == "boolop_swap")
            .expect("expected boolop mutation");

        assert_eq!(mutation.original, "and");
        assert_eq!(mutation.replacement, "or");

        let mutated = apply_mutation(&fm.source, mutation);
        assert!(
            parse_module(&mutated, None).is_ok(),
            "tree-sitter boolop mutation must preserve parseability:\n{mutated}"
        );
        assert!(mutated.contains("or info_name is not None"));
    }

    #[test]
    fn tree_sitter_arg_removal_handles_multiline_calls() {
        let source = concat!(
            "def f(x, y):\n",
            "    return target(\n",
            "        x,\n",
            "        y,\n",
            "    )\n",
        );

        let fms = collect_file_mutations_tree_sitter(source);
        let fm = &fms[0];
        check_span_invariant(fm);

        let mutations: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "arg_removal")
            .collect();

        assert!(!mutations.is_empty(), "expected arg_removal mutations");
        for m in mutations {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "tree-sitter arg_removal mutation must preserve parseability:\n{mutated}"
            );
        }
    }

    #[test]
    fn tree_sitter_skips_decorated_functions() {
        // INV-3: decorated functions produce zero mutations
        let source = "@decorator\ndef f():\n    return 1\n";
        assert!(
            collect_file_mutations_tree_sitter(source).is_empty(),
            "decorated function must produce zero FunctionMutations"
        );
    }

    #[test]
    fn tree_sitter_match_case_removal_preserves_parseability() {
        let source = concat!(
            "def f(value):\n",
            "    match value:\n",
            "        case 1:\n",
            "            return \"one\"\n",
            "        case 2:\n",
            "            return \"two\"\n",
            "        case _:\n",
            "            return \"other\"\n",
        );

        let fms = collect_file_mutations_tree_sitter(source);
        let fm = &fms[0];
        check_span_invariant(fm);

        let mutations: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
            .collect();

        assert_eq!(mutations.len(), 3);
        for m in mutations {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "tree-sitter match_case_removal must preserve parseability:\n{mutated}"
            );
        }
    }

    #[test]
    fn tree_sitter_default_arg_and_name_mutations_work() {
        let source =
            "def f(flag=True, value=0, copier=deepcopy):\n    return flag, value, copier\n";
        let fms = collect_file_mutations_tree_sitter(source);
        let fm = &fms[0];
        check_span_invariant(fm);
        assert!(
            fm.mutations.iter().any(|m| m.operator == "default_arg"),
            "expected default_arg mutation"
        );
    }

    #[test]
    fn tree_sitter_augassign_and_loop_mutations_parse() {
        let source = concat!(
            "def f(items, total):\n",
            "    for item in items:\n",
            "        total += item\n",
            "    while total > 0:\n",
            "        break\n",
            "    return total\n",
        );
        let fms = collect_file_mutations_tree_sitter(source);
        let fm = &fms[0];
        check_span_invariant(fm);

        let operators = ["augop_swap", "augassign_to_assign", "loop_mutation", "keyword_swap"];
        for operator in operators {
            let m = fm
                .mutations
                .iter()
                .find(|m| m.operator == operator)
                .unwrap_or_else(|| panic!("missing {operator} mutation"));
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "{operator} must parse:\n{mutated}"
            );
        }
    }

    #[test]
    fn tree_sitter_exception_and_method_mutations_parse() {
        let source = concat!(
            "def f(text):\n",
            "    try:\n",
            "        return text.split(',', maxsplit=1)\n",
            "    except ValueError:\n",
            "        return dict(foo=1)\n",
        );
        let fms = collect_file_mutations_tree_sitter(source);
        let fm = &fms[0];
        check_span_invariant(fm);

        for operator in ["method_swap", "exception_type", "dict_kwarg"] {
            let m = fm
                .mutations
                .iter()
                .find(|m| m.operator == operator)
                .unwrap_or_else(|| panic!("missing {operator} mutation"));
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "{operator} must parse:\n{mutated}"
            );
        }
    }

    #[test]
    fn never_mutate_functions_produce_no_mutations() {
        // INV-4: NEVER_MUTATE_FUNCTIONS produce zero mutations
        for name in NEVER_MUTATE_FUNCTIONS {
            let source = format!("def {name}(self, x):\n    return x + 1\n");
            let fms = collect_file_mutations_tree_sitter(&source);
            assert!(
                fms.is_empty(),
                "NEVER_MUTATE_FUNCTIONS: {name} must produce zero FunctionMutations"
            );
        }
    }

    // --- Enum skipping ---

    #[test]
    fn tree_sitter_skips_enum_class_methods() {
        let source = "class Color(IntEnum):\n    RED = 1\n    def label(self):\n        return 'color'\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(fms.is_empty(), "Enum subclass methods should produce zero mutations");
    }

    #[test]
    fn tree_sitter_skips_qualified_enum_base() {
        let source = "class Status(enum.StrEnum):\n    ACTIVE = 'active'\n    def display(self):\n        return self.value\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(fms.is_empty());
    }

    #[test]
    fn tree_sitter_does_not_skip_regular_classes() {
        let source = "class Foo(Base):\n    def bar(self):\n        return 1\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(!fms.is_empty(), "Regular class methods should still be mutated");
    }

    #[test]
    fn tree_sitter_skips_all_enum_variants() {
        for base in &["Enum", "IntEnum", "StrEnum", "Flag", "IntFlag", "enum.Enum", "enum.IntEnum", "enum.StrEnum", "enum.Flag", "enum.IntFlag"] {
            let source = format!("class C({base}):\n    A = 1\n    def f(self):\n        return 1\n");
            let fms = collect_file_mutations_tree_sitter(&source);
            assert!(fms.is_empty(), "Expected empty for base {base}, got {:?}", fms.len());
        }
    }

    // --- Nonlocal detection ---

    #[test]
    fn tree_sitter_skips_function_with_nonlocal() {
        let source = "def outer():\n    x = 0\n    def inner():\n        nonlocal x\n        x += 1\n    return inner\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(fms.is_empty(), "Function containing nonlocal should be skipped");
    }

    #[test]
    fn tree_sitter_skips_direct_nonlocal() {
        // nonlocal at function body level (not nested) should also be skipped
        let source = "def f():\n    nonlocal x\n    return x + 1\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(fms.is_empty());
    }

    // --- NEVER_MUTATE_FUNCTION_CALLS ---

    #[test]
    fn tree_sitter_skips_len_arg_removal() {
        let source = "def f(items):\n    return len(items)\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(
            !fms.is_empty(),
            "f(items) with return should produce mutations"
        );
        assert!(
            !fms[0].mutations.iter().any(|m| m.operator == "arg_removal"),
            "len() should not have arg_removal mutations"
        );
    }

    #[test]
    fn tree_sitter_skips_isinstance_arg_removal() {
        let source = "def f(x):\n    return isinstance(x, int)\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(
            !fms.is_empty(),
            "f(x) with return should produce mutations"
        );
        assert!(
            !fms[0].mutations.iter().any(|m| m.operator == "arg_removal"),
            "isinstance() should not have arg_removal mutations"
        );
    }

    #[test]
    fn tree_sitter_does_mutate_non_filtered_calls() {
        // A user-defined function call should still get arg_removal mutations.
        let source = "def f(x, y):\n    return foo(x, y)\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(!fms.is_empty());
        assert!(
            fms[0].mutations.iter().any(|m| m.operator == "arg_removal"),
            "foo() should have arg_removal mutations"
        );
    }

    #[test]
    fn tree_sitter_skips_arg_removal_for_generator_expression() {
        // tuple(expr for x in items) — the genexpr is the sole arg with no parens.
        // Replacing it with None produces invalid syntax: tuple(None, for x in items)
        let source = "def f(value):\n    return tuple(helper(x) for x in value)\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(!fms.is_empty());
        // The outer tuple() call must NOT get arg_removal (genexpr),
        // but the inner helper(x) call CAN get arg_removal (normal call).
        assert!(
            !fms[0].mutations.iter().any(|m| {
                m.operator == "arg_removal" && m.original.starts_with("tuple(")
            }),
            "tuple() with generator expression should not have arg_removal mutations"
        );
    }

    #[test]
    fn tree_sitter_arg_removal_ignores_inline_comments() {
        // Comments inside argument lists (e.g. `# type: ignore`) are tree-sitter
        // "extra" nodes. If collected as arguments, arg_removal produces invalid
        // code like `foo(# type: ignore, None, y)` where the `#` swallows the `)`.
        let source = "def f(ctx, val):\n    return bar(  # type: ignore\n        ctx, val\n    )\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(!fms.is_empty());
        for m in &fms[0].mutations {
            if m.operator == "arg_removal" {
                assert!(
                    !m.replacement.contains('#'),
                    "arg_removal must not include comments in replacement: {}",
                    m.replacement
                );
            }
        }
    }

    // --- Line span tests ---

    #[test]
    fn line_span_single_function_at_top_of_file() {
        // INV-2: first line of file is 1; INV-1: start_line <= end_line
        let source = "def f(x):\n    return x + 1\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        assert_eq!(fm.start_line, 1, "function starts on line 1");
        assert_eq!(fm.end_line, 2, "function ends on line 2");
        assert!(fm.start_line <= fm.end_line, "INV-1: start_line <= end_line");
    }

    #[test]
    fn line_span_function_not_at_line_one() {
        // Function that starts on line 3 (two blank lines precede it)
        let source = "\n\ndef g(a, b):\n    return a + b\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        assert_eq!(fm.start_line, 3, "function starts on line 3");
        assert_eq!(fm.end_line, 4, "function ends on line 4");
        assert!(fm.start_line <= fm.end_line, "INV-1: start_line <= end_line");
    }

    #[test]
    fn line_spans_multiple_functions() {
        // Two functions; verify each gets independent correct spans.
        let source = concat!(
            "def first(x):\n",   // line 1
            "    return x + 1\n", // line 2
            "\n",                  // line 3
            "def second(y):\n",   // line 4
            "    return y - 1\n", // line 5
        );
        let fms = collect_file_mutations_tree_sitter(source);
        assert_eq!(fms.len(), 2, "expected two functions");

        let first = fms.iter().find(|f| f.name == "first").expect("first");
        let second = fms.iter().find(|f| f.name == "second").expect("second");

        assert_eq!(first.start_line, 1);
        assert_eq!(first.end_line, 2);
        assert_eq!(second.start_line, 4);
        assert_eq!(second.end_line, 5);

        assert!(first.start_line <= first.end_line, "INV-1 for first");
        assert!(second.start_line <= second.end_line, "INV-1 for second");
        assert!(first.end_line < second.start_line, "functions must not overlap");
    }

    #[test]
    fn line_span_class_method() {
        // Method inside a class; line numbers are absolute in the source file.
        let source = concat!(
            "class Foo:\n",       // line 1
            "    def bar(self):\n", // line 2
            "        return 1\n",  // line 3
        );
        let fms = collect_file_mutations_tree_sitter(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        assert_eq!(fm.name, "bar");
        assert_eq!(fm.start_line, 2, "method starts on line 2");
        assert_eq!(fm.end_line, 3, "method ends on line 3");
        assert!(fm.start_line <= fm.end_line, "INV-1 for method");
    }

    #[test]
    fn line_span_multiline_function_body() {
        // Longer function; end_line must be the last line of the def block.
        let source = concat!(
            "def compute(a, b, c):\n", // line 1
            "    x = a + b\n",          // line 2
            "    y = b - c\n",          // line 3
            "    return x + y\n",       // line 4
        );
        let fms = collect_file_mutations_tree_sitter(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        assert_eq!(fm.start_line, 1);
        assert_eq!(fm.end_line, 4);
        assert!(fm.start_line <= fm.end_line, "INV-1");
    }

}
