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
        .map(|node| node_text(source, node).to_string())
        .unwrap_or_default();
    let return_annotation = function_node
        .child_by_field_name("return_type")
        .map(|node| format!(" -> {}", node_text(source, node)))
        .unwrap_or_default();
    let is_async = source[fn_start..name_node.start_byte()].contains("async");
    let is_generator =
        subtree_contains_kind(body, "yield") || subtree_contains_kind(body, "yield_from");

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

    Some(FunctionMutations {
        name,
        class_name: class_name.map(ToOwned::to_owned),
        source: func_source.to_string(),
        params_source,
        return_annotation,
        is_async,
        is_generator,
        mutations,
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
        "lambda" => add_lambda_mutation(node, source, fn_start, mutations),
        "conditional_expression" => add_ternary_mutations(node, source, fn_start, mutations),
        "raise_statement" => add_raise_deletion(node, source, fn_start, mutations),
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
    if value_node.kind() == "string" {
        return;
    }

    let stmt_text = node_text(source, node);
    record_mutation(
        stmt_text,
        "pass",
        "statement_deletion",
        node.start_byte() - fn_start,
        mutations,
    );
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
    let assignment_text = node_text(source, node);
    let value_text = node_text(source, value_node);
    let replacement_value = if value_text.trim() == "None" {
        "\"\""
    } else {
        "None"
    };
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

    if !assignment_text.trim_start().starts_with("self.") {
        record_mutation(
            assignment_text,
            "pass",
            "statement_deletion",
            node.start_byte() - fn_start,
            mutations,
        );
    }
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
    let text = node_text(source, value);
    if text.trim() != "Exception" {
        record_mutation(
            text,
            "Exception",
            "exception_type",
            value.start_byte() - fn_start,
            mutations,
        );
    }
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

/// Replace the body of a lambda expression with a different value.
///
/// `lambda x: body` → `lambda x: None` (when body ≠ None)
/// `lambda x: None` → `lambda x: 0`
fn add_lambda_mutation(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let body_text = node_text(source, body);
    let replacement_body = if body_text.trim() == "None" { "0" } else { "None" };

    let full_text = node_text(source, node);
    let body_rel_start = body.start_byte() - node.start_byte();
    let body_rel_end = body.end_byte() - node.start_byte();
    let replacement = format!(
        "{}{}{}",
        &full_text[..body_rel_start],
        replacement_body,
        &full_text[body_rel_end..]
    );
    record_mutation(
        full_text,
        &replacement,
        "lambda_mutation",
        node.start_byte() - fn_start,
        mutations,
    );
}

/// Swap the body and alternative of a conditional (ternary) expression.
///
/// `a if cond else b` → `b if cond else a`
///
/// Also adds condition_negation on the test expression.
///
/// Note: in tree-sitter-python 0.23, `conditional_expression` children are positional
/// (no field names): child[0]=body (true branch), child[1]=condition, child[2]=alternative.
fn add_ternary_mutations(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    // Collect children into a Vec to avoid cursor lifetime issues (see gotcha note).
    let children: Vec<Node<'_>> = {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).collect()
    };
    if children.len() < 3 {
        return;
    }
    let body = children[0];
    let condition = children[1];
    let alternative = children[2];

    let body_text = node_text(source, body);
    let cond_text = node_text(source, condition);
    let alt_text = node_text(source, alternative);

    // Ternary swap: a if cond else b → b if cond else a
    if body_text != alt_text {
        let replacement = format!("{alt_text} if {cond_text} else {body_text}");
        record_mutation(
            node_text(source, node),
            &replacement,
            "ternary_swap",
            node.start_byte() - fn_start,
            mutations,
        );
    }

    // Condition negation on the ternary test expression
    if condition.kind() != "not_operator" {
        let neg_replacement = format!("not ({cond_text})");
        record_mutation(
            cond_text,
            &neg_replacement,
            "condition_negation",
            condition.start_byte() - fn_start,
            mutations,
        );
    }
}

/// Delete explicit raise statements (replace with `pass`).
///
/// `raise ValueError("bad")` → `pass`
///
/// Bare `raise` (re-raise inside except) is not deleted.
fn add_raise_deletion(
    node: Node<'_>,
    source: &str,
    fn_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    // Skip bare raise (re-raise) — it has no named children
    let mut cursor = node.walk();
    if node.named_children(&mut cursor).next().is_none() {
        return;
    }
    record_mutation(
        node_text(source, node),
        "pass",
        "statement_deletion",
        node.start_byte() - fn_start,
        mutations,
    );
}

fn collect_call_arguments<'a>(args_node: Node<'a>, source: &'a str) -> Vec<CallArg<'a>> {
    let mut args = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.named_children(&mut cursor) {
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
                    if replacement != value_text {
                        record_mutation(
                            value_text,
                            replacement,
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

fn compute_default_replacement(default_text: &str) -> Option<&'static str> {
    match default_text.trim() {
        "True" => Some("False"),
        "False" => Some("True"),
        "None" => Some("\"\""),
        "0" => Some("1"),
        "1" => Some("0"),
        "\"\"" | "''" => Some("None"),
        _ => {
            if default_text.starts_with('"') || default_text.starts_with('\'') {
                Some("\"\"")
            } else {
                Some("None")
            }
        }
    }
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

    // --- New operator tests ---

    #[test]
    fn tree_sitter_lambda_mutation() {
        let source = "def f():\n    g = lambda x: x + 1\n    return g\n";
        let fms = collect_file_mutations_tree_sitter(source);
        let fm = &fms[0];
        let mutation = fm
            .mutations
            .iter()
            .find(|m| m.operator == "lambda_mutation")
            .expect("expected lambda_mutation");
        assert!(mutation.replacement.contains("None"));
        let mutated = apply_mutation(&fm.source, mutation);
        assert!(parse_module(&mutated, None).is_ok(), "lambda mutation must parse: {mutated}");
    }

    #[test]
    fn tree_sitter_lambda_none_body_mutates_to_zero() {
        let source = "def f():\n    g = lambda: None\n    return g\n";
        let fms = collect_file_mutations_tree_sitter(source);
        let mutation = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "lambda_mutation")
            .expect("expected lambda_mutation");
        assert!(mutation.replacement.contains("0"));
    }

    #[test]
    fn tree_sitter_ternary_swap() {
        let source = "def f(x):\n    return 'yes' if x else 'no'\n";
        let fms = collect_file_mutations_tree_sitter(source);
        let fm = &fms[0];

        let swap = fm
            .mutations
            .iter()
            .find(|m| m.operator == "ternary_swap")
            .expect("expected ternary_swap");
        let mutated = apply_mutation(&fm.source, swap);
        assert!(parse_module(&mutated, None).is_ok(), "ternary swap must parse: {mutated}");
        assert!(mutated.contains("'no' if") && mutated.contains("else 'yes'"));

        // Also check condition_negation on ternary
        assert!(fm.mutations.iter().any(|m| m.operator == "condition_negation"));
    }

    #[test]
    fn tree_sitter_ternary_swap_skips_identical() {
        let source = "def f(x):\n    return x if x else x\n";
        let fms = collect_file_mutations_tree_sitter(source);
        assert!(
            !fms[0].mutations.iter().any(|m| m.operator == "ternary_swap"),
            "ternary swap should be skipped when body == alternative"
        );
    }

    #[test]
    fn tree_sitter_raise_deletion() {
        let source =
            "def f(x):\n    if not x:\n        raise ValueError('bad')\n    return x\n";
        let fms = collect_file_mutations_tree_sitter(source);
        let fm = &fms[0];
        let mutation = fm
            .mutations
            .iter()
            .find(|m| m.operator == "statement_deletion" && m.original.contains("raise"))
            .expect("expected raise deletion");
        assert_eq!(mutation.replacement, "pass");
        let mutated = apply_mutation(&fm.source, mutation);
        assert!(parse_module(&mutated, None).is_ok(), "raise deletion must parse: {mutated}");
    }

    #[test]
    fn tree_sitter_bare_raise_not_deleted() {
        let source = "def f():\n    try:\n        pass\n    except:\n        raise\n";
        let fms = collect_file_mutations_tree_sitter(source);
        // bare raise should NOT produce a statement_deletion targeting the raise
        let has_raise_deletion = fms
            .iter()
            .flat_map(|fm| &fm.mutations)
            .any(|m| m.operator == "statement_deletion" && m.original.trim() == "raise");
        assert!(!has_raise_deletion, "bare raise should not be deleted");
    }
}
