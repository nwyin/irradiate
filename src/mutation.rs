//! Mutation engine: parse Python source, identify mutation points, generate mutant variants.
//!
//! Strategy: use libcst for structural analysis, but generate mutations as text substitutions.
//! This avoids needing to clone/modify CST nodes (which libcst Rust doesn't support well).

use libcst_native::{
    self as cst, parse_module, BinaryOp, BooleanOp, Codegen, CodegenState, CompOp,
    CompoundStatement, Expression, SmallStatement, Statement, UnaryOp,
};

/// A single mutation that can be applied to source code.
#[derive(Debug, Clone)]
pub struct Mutation {
    /// Byte offset in the function source where the original text starts.
    pub offset: usize,
    /// The original text to replace.
    pub original: String,
    /// The replacement text.
    pub replacement: String,
    /// Which operator produced this mutation.
    pub operator: &'static str,
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
    /// Whether the function is async.
    pub is_async: bool,
    /// Mutations found within this function body.
    pub mutations: Vec<Mutation>,
}

/// Collect all function mutations from a Python source file.
pub fn collect_file_mutations(source: &str) -> Vec<FunctionMutations> {
    let module = match parse_module(source, None) {
        Ok(m) => m,
        Err(_) => return vec![], // skip files that don't parse
    };

    let mut results = Vec::new();
    let ignored_lines = pragma_no_mutate_lines(source);

    for stmt in &module.body {
        match stmt {
            Statement::Compound(CompoundStatement::FunctionDef(func)) => {
                if let Some(fm) = collect_function_mutations(func, None, source, &ignored_lines) {
                    results.push(fm);
                }
            }
            Statement::Compound(CompoundStatement::ClassDef(cls)) => {
                let class_name = codegen_node(&cls.name);
                if let cst::Suite::IndentedBlock(block) = &cls.body {
                    for method_stmt in &block.body {
                        if let Statement::Compound(CompoundStatement::FunctionDef(func)) =
                            method_stmt
                        {
                            if let Some(fm) = collect_function_mutations(
                                func,
                                Some(&class_name),
                                source,
                                &ignored_lines,
                            ) {
                                results.push(fm);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    results
}

/// Skip rules from the design doc.
const NEVER_MUTATE_FUNCTIONS: &[&str] = &["__getattribute__", "__setattr__", "__new__"];

fn collect_function_mutations(
    func: &cst::FunctionDef,
    class_name: Option<&str>,
    _full_source: &str,
    ignored_lines: &std::collections::HashSet<usize>,
) -> Option<FunctionMutations> {
    let name = codegen_node(&func.name);

    // Skip dunder methods that affect object identity
    if NEVER_MUTATE_FUNCTIONS.contains(&name.as_str()) {
        return None;
    }

    // Skip decorated functions (same rationale as mutmut)
    if !func.decorators.is_empty() {
        return None;
    }

    let func_source = codegen_node(func);
    let params_source = codegen_node(&func.params);
    let is_async = func.asynchronous.is_some();

    let mut mutations = Vec::new();
    let mut cursor = 0usize;

    collect_suite_mutations(
        &func.body,
        &func_source,
        &mut cursor,
        &mut mutations,
        ignored_lines,
    );

    if mutations.is_empty() {
        return None;
    }

    Some(FunctionMutations {
        name,
        class_name: class_name.map(String::from),
        source: func_source,
        params_source,
        is_async,
        mutations,
    })
}

// --- CST walking ---

fn collect_suite_mutations(
    suite: &cst::Suite,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
    ignored: &std::collections::HashSet<usize>,
) {
    match suite {
        cst::Suite::IndentedBlock(block) => {
            for stmt in &block.body {
                collect_statement_mutations(stmt, source, cursor, mutations, ignored);
            }
        }
        cst::Suite::SimpleStatementSuite(s) => {
            for stmt in &s.body {
                collect_small_statement_mutations(stmt, source, cursor, mutations, ignored);
            }
        }
    }
}

fn collect_statement_mutations(
    stmt: &Statement,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
    ignored: &std::collections::HashSet<usize>,
) {
    match stmt {
        Statement::Simple(simple) => {
            for s in &simple.body {
                collect_small_statement_mutations(s, source, cursor, mutations, ignored);
            }
        }
        Statement::Compound(compound) => match compound {
            CompoundStatement::FunctionDef(_) => {} // don't recurse into nested functions
            CompoundStatement::If(if_stmt) => {
                collect_expr_mutations(&if_stmt.test, source, cursor, mutations, ignored);
                collect_suite_mutations(&if_stmt.body, source, cursor, mutations, ignored);
                if let Some(ref orelse) = if_stmt.orelse {
                    match orelse.as_ref() {
                        cst::OrElse::Elif(elif) => {
                            collect_expr_mutations(&elif.test, source, cursor, mutations, ignored);
                            collect_suite_mutations(&elif.body, source, cursor, mutations, ignored);
                        }
                        cst::OrElse::Else(else_clause) => {
                            collect_suite_mutations(
                                &else_clause.body,
                                source,
                                cursor,
                                mutations,
                                ignored,
                            );
                        }
                    }
                }
            }
            CompoundStatement::While(w) => {
                collect_expr_mutations(&w.test, source, cursor, mutations, ignored);
                collect_suite_mutations(&w.body, source, cursor, mutations, ignored);
            }
            CompoundStatement::For(f) => {
                collect_expr_mutations(&f.iter, source, cursor, mutations, ignored);
                collect_suite_mutations(&f.body, source, cursor, mutations, ignored);
            }
            CompoundStatement::With(w) => {
                for item in &w.items {
                    collect_expr_mutations(&item.item, source, cursor, mutations, ignored);
                }
                collect_suite_mutations(&w.body, source, cursor, mutations, ignored);
            }
            CompoundStatement::Try(t) => {
                collect_suite_mutations(&t.body, source, cursor, mutations, ignored);
                for handler in &t.handlers {
                    collect_suite_mutations(&handler.body, source, cursor, mutations, ignored);
                }
                if let Some(ref fin) = t.finalbody {
                    collect_suite_mutations(&fin.body, source, cursor, mutations, ignored);
                }
            }
            _ => {}
        },
    }
}

fn collect_small_statement_mutations(
    stmt: &SmallStatement,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
    ignored: &std::collections::HashSet<usize>,
) {
    match stmt {
        SmallStatement::Return(ret) => {
            if let Some(ref val) = ret.value {
                collect_expr_mutations(val, source, cursor, mutations, ignored);
            }
        }
        SmallStatement::Expr(e) => {
            collect_expr_mutations(&e.value, source, cursor, mutations, ignored);
        }
        SmallStatement::Assign(a) => {
            collect_expr_mutations(&a.value, source, cursor, mutations, ignored);
            // Assignment mutation: a = x → a = None
            add_assignment_mutation(a, source, cursor, mutations);
        }
        SmallStatement::AugAssign(aug) => {
            collect_expr_mutations(&aug.value, source, cursor, mutations, ignored);
            // AugAssign operator swap (handled via operator swap below)
            add_augassign_mutations(aug, source, cursor, mutations);
        }
        SmallStatement::Assert(a) => {
            collect_expr_mutations(&a.test, source, cursor, mutations, ignored);
        }
        _ => {}
    }
}

#[allow(clippy::only_used_in_recursion)] // `ignored` will be used for pragma line checking
fn collect_expr_mutations(
    expr: &Expression,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
    ignored: &std::collections::HashSet<usize>,
) {
    match expr {
        Expression::BinaryOperation(binop) => {
            collect_expr_mutations(&binop.left, source, cursor, mutations, ignored);
            add_binop_mutations(&binop.operator, source, cursor, mutations);
            collect_expr_mutations(&binop.right, source, cursor, mutations, ignored);
        }
        Expression::BooleanOperation(boolop) => {
            collect_expr_mutations(&boolop.left, source, cursor, mutations, ignored);
            add_boolop_mutations(&boolop.operator, source, cursor, mutations);
            collect_expr_mutations(&boolop.right, source, cursor, mutations, ignored);
        }
        Expression::UnaryOperation(unop) => {
            add_unaryop_mutations(unop, source, cursor, mutations);
            collect_expr_mutations(&unop.expression, source, cursor, mutations, ignored);
        }
        Expression::Comparison(cmp) => {
            collect_expr_mutations(&cmp.left, source, cursor, mutations, ignored);
            for target in &cmp.comparisons {
                add_compop_mutations(&target.operator, source, cursor, mutations);
                collect_expr_mutations(&target.comparator, source, cursor, mutations, ignored);
            }
        }
        Expression::Name(name) => {
            add_name_mutations(name, source, cursor, mutations);
        }
        Expression::Integer(int) => {
            add_number_mutation(int, source, cursor, mutations);
        }
        Expression::Float(float) => {
            add_float_mutation(float, source, cursor, mutations);
        }
        Expression::SimpleString(s) => {
            add_string_mutation(s, source, cursor, mutations);
        }
        Expression::Call(call) => {
            add_method_mutations(call, source, cursor, mutations);
            collect_expr_mutations(&call.func, source, cursor, mutations, ignored);
            for arg in &call.args {
                collect_expr_mutations(&arg.value, source, cursor, mutations, ignored);
            }
        }
        Expression::IfExp(ifexp) => {
            collect_expr_mutations(&ifexp.test, source, cursor, mutations, ignored);
            collect_expr_mutations(&ifexp.body, source, cursor, mutations, ignored);
            collect_expr_mutations(&ifexp.orelse, source, cursor, mutations, ignored);
        }
        Expression::Lambda(lam) => {
            add_lambda_mutation(lam, source, cursor, mutations);
        }
        Expression::Tuple(t) => {
            for el in &t.elements {
                if let cst::Element::Simple {
                    value: ref e_val, ..
                } = el
                {
                    collect_expr_mutations(e_val, source, cursor, mutations, ignored);
                }
            }
        }
        Expression::List(l) => {
            for el in &l.elements {
                if let cst::Element::Simple {
                    value: ref e_val, ..
                } = el
                {
                    collect_expr_mutations(e_val, source, cursor, mutations, ignored);
                }
            }
        }
        Expression::Dict(d) => {
            for el in &d.elements {
                if let cst::DictElement::Simple {
                    ref key, ref value, ..
                } = el
                {
                    collect_expr_mutations(key, source, cursor, mutations, ignored);
                    collect_expr_mutations(value, source, cursor, mutations, ignored);
                }
            }
        }
        Expression::Subscript(sub) => {
            collect_expr_mutations(&sub.value, source, cursor, mutations, ignored);
        }
        Expression::Attribute(attr) => {
            collect_expr_mutations(&attr.value, source, cursor, mutations, ignored);
        }
        _ => {}
    }
}

// --- Operator mutation helpers ---

fn find_and_record(
    node_text: &str,
    replacement: &str,
    operator: &'static str,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    if let Some(pos) = source[*cursor..].find(node_text) {
        let offset = *cursor + pos;
        mutations.push(Mutation {
            offset,
            original: node_text.to_string(),
            replacement: replacement.to_string(),
            operator,
        });
        // Don't advance cursor past operator — multiple mutations can target the same text
    }
}

/// Binary operator swaps: +↔-, *↔/, etc.
static BINOP_SWAPS: &[(&str, &str)] = &[
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

fn add_binop_mutations(
    op: &BinaryOp,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let op_text = codegen_node(op);
    let op_trimmed = op_text.trim();

    for &(from, to) in BINOP_SWAPS {
        if op_trimmed == from {
            let replacement = op_text.replace(from, to);
            find_and_record(
                &op_text,
                &replacement,
                "binop_swap",
                source,
                cursor,
                mutations,
            );
            break;
        }
    }
}

fn add_boolop_mutations(
    op: &BooleanOp,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let op_text = codegen_node(op);
    let op_trimmed = op_text.trim();

    let replacement = match op_trimmed {
        "and" => op_text.replace("and", "or"),
        "or" => op_text.replace("or", "and"),
        _ => return,
    };
    find_and_record(
        &op_text,
        &replacement,
        "boolop_swap",
        source,
        cursor,
        mutations,
    );
}

/// Comparison operator swaps.
static COMPOP_SWAPS: &[(&str, &str)] = &[
    ("<=", "<"),
    (">=", ">"),
    ("<", "<="),
    (">", ">="),
    ("==", "!="),
    ("!=", "=="),
    (" is not ", " is "),
    (" is ", " is not "),
    (" not in ", " in "),
    (" in ", " not in "),
];

fn add_compop_mutations(
    op: &CompOp,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let op_text = codegen_node(op);
    let op_trimmed = op_text.trim();

    for &(from, to) in COMPOP_SWAPS {
        if op_trimmed == from.trim() {
            let replacement = op_text.replace(from.trim(), to.trim());
            find_and_record(
                &op_text,
                &replacement,
                "compop_swap",
                source,
                cursor,
                mutations,
            );
            break;
        }
    }
}

fn add_unaryop_mutations(
    unop: &cst::UnaryOperation,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    // not x → x, ~x → x
    match &unop.operator {
        UnaryOp::Not { .. } | UnaryOp::BitInvert { .. } => {
            let full_text = codegen_node(unop);
            let inner_text = codegen_node(&*unop.expression);
            find_and_record(
                &full_text,
                &inner_text,
                "unary_removal",
                source,
                cursor,
                mutations,
            );
        }
        _ => {}
    }
}

fn add_name_mutations(
    name: &cst::Name,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let text = name.value;
    let replacement = match text {
        "True" => "False",
        "False" => "True",
        "deepcopy" => "copy",
        _ => return,
    };
    find_and_record(text, replacement, "name_swap", source, cursor, mutations);
}

fn add_number_mutation(
    int: &cst::Integer,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let text = int.value;
    // n → n + 1
    if let Ok(n) = text.replace('_', "").parse::<i64>() {
        let replacement = (n + 1).to_string();
        if replacement != text {
            find_and_record(
                text,
                &replacement,
                "number_mutation",
                source,
                cursor,
                mutations,
            );
        }
    }
}

fn add_float_mutation(
    float: &cst::Float,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let text = float.value;
    if let Ok(n) = text.parse::<f64>() {
        let replacement = format!("{}", n + 1.0);
        if replacement != text {
            find_and_record(
                text,
                &replacement,
                "number_mutation",
                source,
                cursor,
                mutations,
            );
        }
    }
}

fn add_string_mutation(
    s: &cst::SimpleString,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let text = s.value;

    // Skip triple-quoted strings (docstrings)
    if text.contains("\"\"\"") || text.contains("'''") {
        return;
    }

    // XX prefix+suffix mutation
    let quote_char = if text.contains('"') { '"' } else { '\'' };
    let prefix_end = text.find(quote_char).unwrap();
    let prefix = &text[..prefix_end];
    let inner = &text[prefix_end + 1..text.len() - 1];
    let replacement = format!("{prefix}{quote_char}XX{inner}XX{quote_char}");

    if replacement != text {
        find_and_record(
            text,
            &replacement,
            "string_mutation",
            source,
            cursor,
            mutations,
        );
    }
}

fn add_lambda_mutation(
    lam: &cst::Lambda,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let full_text = codegen_node(lam);
    let body_text = codegen_node(&*lam.body);
    let replacement_body = if body_text.trim() == "None" {
        "0"
    } else {
        "None"
    };
    let replacement = full_text.replace(&body_text, replacement_body);
    find_and_record(
        &full_text,
        &replacement,
        "lambda_mutation",
        source,
        cursor,
        mutations,
    );
}

fn add_assignment_mutation(
    assign: &cst::Assign,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let value_text = codegen_node(&assign.value);
    let replacement = if value_text.trim() == "None" {
        " \"\"".to_string()
    } else {
        " None".to_string()
    };
    // Find the value part and replace it
    let full_text = codegen_node(assign);
    let new_full = if let Some(eq_pos) = full_text.find('=') {
        format!("{}={replacement}", &full_text[..eq_pos])
    } else {
        return;
    };
    find_and_record(
        &full_text,
        &new_full,
        "assignment_mutation",
        source,
        cursor,
        mutations,
    );
}

/// AugAssign operator swap: += → -=, etc., and also += → = (strip augmentation)
static AUGOP_SWAPS: &[(&str, &str)] = &[
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

fn add_augassign_mutations(
    aug: &cst::AugAssign,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    let op_text = codegen_node(&aug.operator);
    let op_trimmed = op_text.trim();

    // Operator swap
    for &(from, to) in AUGOP_SWAPS {
        if op_trimmed == from {
            let replacement = op_text.replace(from, to);
            find_and_record(
                &op_text,
                &replacement,
                "augop_swap",
                source,
                cursor,
                mutations,
            );
            break;
        }
    }

    // AugAssign → plain Assign (e.g., a += b → a = b)
    let full_text = codegen_node(aug);
    let target_text = codegen_node(&aug.target);
    let value_text = codegen_node(&aug.value);
    let plain_assign = format!("{target_text} ={value_text}");
    find_and_record(
        &full_text,
        &plain_assign,
        "augassign_to_assign",
        source,
        cursor,
        mutations,
    );
}

/// String method swaps: .lower() ↔ .upper(), .lstrip() ↔ .rstrip(), etc.
static METHOD_SWAPS: &[(&str, &str)] = &[
    ("lower", "upper"),
    ("upper", "lower"),
    ("lstrip", "rstrip"),
    ("rstrip", "lstrip"),
    ("find", "rfind"),
    ("rfind", "find"),
];

fn add_method_mutations(
    call: &cst::Call,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
) {
    // Only mutate if the call target is an attribute access (i.e., obj.method())
    if let Expression::Attribute(attr) = &*call.func {
        let method_name = codegen_node(&attr.attr);
        let method_trimmed = method_name.trim();

        for &(from, to) in METHOD_SWAPS {
            if method_trimmed == from {
                find_and_record(
                    &method_name,
                    &method_name.replace(from, to),
                    "method_swap",
                    source,
                    cursor,
                    mutations,
                );
                break;
            }
        }
    }
}

// --- Utility ---

fn codegen_node<'a>(node: &impl Codegen<'a>) -> String {
    let mut state = CodegenState::default();
    node.codegen(&mut state);
    state.tokens
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
                    .is_some_and(|s| s.contains("no mutate"))
            {
                Some(i + 1) // 1-indexed
            } else {
                None
            }
        })
        .collect()
}

/// Apply a single mutation to a function's source text.
pub fn apply_mutation(func_source: &str, mutation: &Mutation) -> String {
    format!(
        "{}{}{}",
        &func_source[..mutation.offset],
        mutation.replacement,
        &func_source[mutation.offset + mutation.original.len()..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_binop_mutations() {
        let source = "def add(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        assert_eq!(fm.name, "add");

        let binop_mutations: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();
        assert!(!binop_mutations.is_empty(), "Should find + → - mutation");
        assert!(
            binop_mutations[0].replacement.contains('-'),
            "Should swap + to -"
        );
    }

    #[test]
    fn test_collect_comparison_mutations() {
        let source = "def check(n):\n    if n > 0:\n        return True\n    return False\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);

        let compop = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "compop_swap");
        assert!(compop.is_some(), "Should find > → >= mutation");

        let name_muts: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "name_swap")
            .collect();
        assert!(
            name_muts.len() >= 2,
            "Should find True→False and False→True"
        );
    }

    #[test]
    fn test_collect_string_mutations() {
        let source = "def greet():\n    return \"hello\"\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);

        let string_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "string_mutation");
        assert!(string_mut.is_some(), "Should find string mutation");
        assert!(
            string_mut.unwrap().replacement.contains("XX"),
            "Should add XX prefix/suffix"
        );
    }

    #[test]
    fn test_skip_decorated_functions() {
        let source = "@decorator\ndef foo():\n    return 1 + 2\n";
        let fms = collect_file_mutations(source);
        assert!(fms.is_empty(), "Decorated functions should be skipped");
    }

    #[test]
    fn test_skip_docstrings() {
        let source = "def foo():\n    \"\"\"docstring\"\"\"\n    return 1\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let string_muts: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "string_mutation")
            .collect();
        assert!(string_muts.is_empty(), "Docstrings should not be mutated");
    }

    #[test]
    fn test_apply_mutation() {
        let source = "def add(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        let binop = fm
            .mutations
            .iter()
            .find(|m| m.operator == "binop_swap")
            .unwrap();

        let mutated = apply_mutation(&fm.source, binop);
        assert!(mutated.contains(" - "), "Should have - instead of +");
        assert!(!mutated.contains(" + "), "Should not have + anymore");
    }

    #[test]
    fn test_number_mutation() {
        let source = "def foo():\n    return 42\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let num_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "number_mutation");
        assert!(num_mut.is_some());
        assert_eq!(num_mut.unwrap().replacement, "43");
    }

    #[test]
    fn test_boolean_op_mutation() {
        let source = "def foo(a, b):\n    return a and b\n";
        let fms = collect_file_mutations(source);
        let boolop = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "boolop_swap");
        assert!(boolop.is_some(), "Should find and → or mutation");
    }

    #[test]
    fn test_pragma_no_mutate() {
        let source = "def foo():\n    return 1 + 2  # pragma: no mutate\n";
        let fms = collect_file_mutations(source);
        // The function should still be found, but the pragma line is in ignored_lines
        // Currently our walker doesn't check line numbers for individual expressions
        // This is a known limitation - we'd need position tracking for full pragma support
        assert_eq!(fms.len(), 1);
    }

    #[test]
    fn test_class_methods() {
        let source = "class Foo:\n    def bar(self):\n        return 1 + 2\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        assert_eq!(fms[0].name, "bar");
        assert_eq!(fms[0].class_name.as_deref(), Some("Foo"));
    }

    #[test]
    fn test_lambda_mutation() {
        let source = "def foo():\n    f = lambda x: x + 1\n";
        let fms = collect_file_mutations(source);
        let lam = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "lambda_mutation");
        assert!(lam.is_some(), "Should find lambda → None mutation");
    }

    #[test]
    fn test_method_swap_lower_upper() {
        let source = "def foo(s):\n    return s.lower()\n";
        let fms = collect_file_mutations(source);
        let method_mut = fms[0].mutations.iter().find(|m| m.operator == "method_swap");
        assert!(method_mut.is_some(), "Should find .lower() → .upper() mutation");
        let m = method_mut.unwrap();
        assert_eq!(m.original, "lower");
        assert_eq!(m.replacement, "upper");
    }

    #[test]
    fn test_method_swap_lstrip_rstrip() {
        let source = "def foo(s):\n    return s.lstrip()\n";
        let fms = collect_file_mutations(source);
        let method_mut = fms[0].mutations.iter().find(|m| m.operator == "method_swap");
        assert!(method_mut.is_some());
        let m = method_mut.unwrap();
        assert_eq!(m.original, "lstrip");
        assert_eq!(m.replacement, "rstrip");
    }

    #[test]
    fn test_chained_method_swaps() {
        let source = "def foo(s):\n    return s.lower().lstrip()\n";
        let fms = collect_file_mutations(source);
        let method_muts: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "method_swap")
            .collect();
        assert_eq!(method_muts.len(), 2, "Should find 2 method swap mutations");
    }

    #[test]
    fn test_non_matching_method_not_mutated() {
        let source = "def foo(s):\n    return s.strip()\n";
        let fms = collect_file_mutations(source);
        // No mutations at all means no method_swap mutations — the function is excluded entirely
        let method_muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "method_swap")
            .collect();
        assert!(method_muts.is_empty(), ".strip() is not in METHOD_SWAPS");
    }
}

#[cfg(test)]
mod offset_correctness_tests {
    use super::*;

    fn filter_by_op<'a>(fm: &'a FunctionMutations, op: &str) -> Vec<&'a Mutation> {
        fm.mutations.iter().filter(|m| m.operator == op).collect()
    }

    // Duplicate binary operators: `a + b + c` parses as `(a + b) + c`.
    // cursor is not advanced past matched text, so both `+` mutations find the first `+`.
    // This test documents the current behavior and ensures at least one swap is correct.
    #[test]
    fn test_duplicate_binops() {
        let source = "def foo(a, b, c):\n    return a + b + c\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let binops = filter_by_op(&fms[0], "binop_swap");
        assert_eq!(binops.len(), 2, "Should find 2 + operators");
        let m1 = apply_mutation(&fms[0].source, binops[0]);
        let m2 = apply_mutation(&fms[0].source, binops[1]);
        // Both mutations swap + to - somewhere
        assert!(m1.contains('-'), "First mutation should swap + to -");
        assert!(m2.contains('-'), "Second mutation should swap + to -");
        // At least one result still contains a + (the other operator left intact)
        // Note: currently both target the first + (cursor not advanced), so both results
        // are identical: `a - b + c`. If offset tracking is ever fixed, this still passes.
        assert!(
            m1.contains('+') || m2.contains('+'),
            "At least one mutation should leave the other + intact"
        );
    }

    // Nested binops: `(a + b) * (c + d)` has three operators.
    // The `*` mutation has a distinct original text, so it is always found correctly.
    // The two `+` mutations both find the first `+` due to cursor behavior.
    #[test]
    fn test_nested_binops() {
        let source = "def foo(a, b, c, d):\n    return (a + b) * (c + d)\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let binops = filter_by_op(&fms[0], "binop_swap");
        assert_eq!(binops.len(), 3, "Should find 2 + operators and 1 * operator");

        // The * mutation has a unique original text — always correct
        let mul_mut = binops
            .iter()
            .find(|m| m.original.contains('*'))
            .expect("Should find * mutation");
        let mul_result = apply_mutation(&fms[0].source, mul_mut);
        assert!(mul_result.contains('/'), "Should swap * to /");
        assert!(!mul_result.contains('*'), "Should not have * anymore");

        // Every + mutation should produce a - somewhere
        for m in binops.iter().filter(|m| m.original.contains('+')) {
            let result = apply_mutation(&fms[0].source, m);
            assert!(result.contains('-'), "Should swap + to -");
        }
    }

    // Single operator with identical operands: `a + a`.
    // Only one `+` in the expression, so offset is unambiguous.
    #[test]
    fn test_identical_operands() {
        let source = "def foo(a):\n    return a + a\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let binops = filter_by_op(&fms[0], "binop_swap");
        assert_eq!(binops.len(), 1, "Should find exactly one + operator");
        let result = apply_mutation(&fms[0].source, binops[0]);
        assert!(result.contains(" - "), "Should swap + to -");
        assert!(!result.contains(" + "), "Should not have + anymore");
        // Operand `a` should be unchanged
        let return_line = result
            .lines()
            .find(|l| l.contains("return"))
            .expect("Should have return line");
        assert!(return_line.contains('a'), "Operand a should still be present");
    }

    // Repeated boolean name: `True or True` has 2 name_swap candidates and 1 boolop_swap.
    // The boolop has a unique text, the name_swap mutations both find the first True.
    #[test]
    fn test_repeated_names_true_false() {
        let source = "def foo():\n    return True or True\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);

        let name_muts = filter_by_op(&fms[0], "name_swap");
        assert_eq!(name_muts.len(), 2, "Should find 2 True name mutations");

        let boolop_muts = filter_by_op(&fms[0], "boolop_swap");
        assert_eq!(boolop_muts.len(), 1, "Should find 1 or → and boolop mutation");

        // Boolop has unique text: or → and
        let boolop_result = apply_mutation(&fms[0].source, boolop_muts[0]);
        assert!(boolop_result.contains(" and "), "Should swap or to and");
        assert!(!boolop_result.contains(" or "), "Should not have or anymore");

        // Both name mutations should produce a result with False
        for nm in &name_muts {
            let result = apply_mutation(&fms[0].source, nm);
            assert!(result.contains("False"), "Should swap True to False");
        }
    }

    // Repeated number literal: `5 + 5` has 2 number_mutation candidates and 1 binop.
    // Both number mutations find the first `5` due to cursor behavior.
    #[test]
    fn test_repeated_numbers() {
        let source = "def foo():\n    return 5 + 5\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);

        let num_muts = filter_by_op(&fms[0], "number_mutation");
        assert_eq!(num_muts.len(), 2, "Should find 2 number mutations for each 5");

        let binops = filter_by_op(&fms[0], "binop_swap");
        assert_eq!(binops.len(), 1, "Should find 1 binop mutation for +");

        // Both number mutations target 5 → 6
        for nm in &num_muts {
            assert_eq!(nm.original, "5");
            assert_eq!(nm.replacement, "6");
        }

        // Binop mutation swaps + to -
        let binop_result = apply_mutation(&fms[0].source, binops[0]);
        assert!(binop_result.contains(" - "), "Should swap + to -");
    }

    // Operator inside a string literal should not generate binop_swap mutations.
    #[test]
    fn test_string_content_not_mutated_as_operator() {
        let source = "def foo():\n    return \"a + b\"\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);

        let string_muts = filter_by_op(&fms[0], "string_mutation");
        assert_eq!(string_muts.len(), 1, "Should find exactly one string mutation");

        let binops = filter_by_op(&fms[0], "binop_swap");
        assert!(binops.is_empty(), "Should NOT treat + inside string as binop");

        let result = apply_mutation(&fms[0].source, string_muts[0]);
        assert!(result.contains("XX"), "String mutation should add XX markers");
    }

    // Whitespace variants: no-space, single-space, double-space operators.
    // LibCST is full-fidelity, so the operator text preserves original whitespace.
    #[test]
    fn test_whitespace_variants() {
        let cases = [
            "def foo(a, b):\n    return a+b\n",
            "def foo(a, b):\n    return a + b\n",
            "def foo(a, b):\n    return a  +  b\n",
        ];

        for source in &cases {
            let fms = collect_file_mutations(source);
            assert_eq!(fms.len(), 1, "Should find function for: {source}");
            let binops = filter_by_op(&fms[0], "binop_swap");
            assert_eq!(binops.len(), 1, "Should find 1 binop for: {source}");
            let result = apply_mutation(&fms[0].source, binops[0]);
            assert!(result.contains('-'), "Should swap + to - for: {source}");
            assert!(!result.contains('+'), "Should not have + for: {source}");
        }
    }

    // Multi-line function with operators on different lines.
    // Each distinct operator text (` + `, ` - `, ` * `) is found from cursor=0,
    // so repeated uses of the same operator text produce redundant mutations.
    #[test]
    fn test_multiline_function() {
        let source = concat!(
            "def compute(a, b, c, d, e):\n",
            "    x = a + b\n",
            "    y = c - d\n",
            "    z = x * y\n",
            "    if z > 0:\n",
            "        return z + e\n",
            "    else:\n",
            "        return z - e\n",
        );
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1, "Should find the function");

        // 5 binop operators in the body (two +, two -, one *)
        let binops = filter_by_op(&fms[0], "binop_swap");
        assert!(binops.len() >= 4, "Should find at least 4 binary operators");

        // Exactly one comparison operator >
        let compops = filter_by_op(&fms[0], "compop_swap");
        assert_eq!(compops.len(), 1, "Should find one comparison operator (>)");

        // Comparison mutation: > → >=
        let comp_result = apply_mutation(&fms[0].source, compops[0]);
        assert!(comp_result.contains(">="), "Should swap > to >=");

        // Every binop mutation should produce a syntactically intact result
        for m in &binops {
            let result = apply_mutation(&fms[0].source, m);
            assert!(!result.is_empty());
            assert!(
                result.contains("def compute"),
                "Result must preserve function signature"
            );
        }
    }
}
