//! Mutation engine: parse Python source, identify mutation points, generate mutant variants.
//!
//! Strategy: use libcst for structural analysis, but generate mutations as text substitutions.
//! This avoids needing to clone/modify CST nodes (which libcst Rust doesn't support well).
//!
//! Position tracking: each Mutation carries exact byte-span (start, end) offsets within the
//! function source string.  These are computed by having `collect_expr_mutations` locate its
//! own start via a forward search from a monotonically-advancing cursor, then advancing the
//! cursor past the full expression text before returning.  Because the cursor only moves
//! forward, the nth occurrence of a duplicated token is always found correctly.

use libcst_native::{
    self as cst, parse_module, BinaryOp, BooleanOp, Codegen, CodegenState, CompOp,
    CompoundStatement, Expression, SmallStatement, Statement, UnaryOp,
};

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
                if let Some(fm) = collect_function_mutations(func, None, &ignored_lines) {
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
                            if let Some(fm) =
                                collect_function_mutations(func, Some(&class_name), &ignored_lines)
                            {
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

    // Start the cursor past the function header (def name(params):) to avoid
    // accidentally matching parameter names or default values when searching
    // for body expressions.
    let body_text = codegen_node(&func.body);
    let body_offset = func_source.len().saturating_sub(body_text.len());
    let mut cursor = body_offset;

    let mut mutations = Vec::new();

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
            // Pre-find the full assignment text before descending so we have its start position.
            let assign_text = codegen_node(a);
            let assign_start = source[*cursor..].find(&assign_text).map(|p| *cursor + p);

            collect_expr_mutations(&a.value, source, cursor, mutations, ignored);

            // Assignment mutation: a = x → a = None
            if let Some(start) = assign_start {
                add_assignment_mutation_at(a, &assign_text, start, mutations);
            }
        }
        SmallStatement::AugAssign(aug) => {
            // Pre-find the full augmented assignment before descending.
            let full_text = codegen_node(aug);
            let aug_start = source[*cursor..].find(&full_text).map(|p| *cursor + p);

            // The operator immediately follows the target.
            let target_text = codegen_node(&aug.target);
            let op_text = codegen_node(&aug.operator);
            let op_start = aug_start.map(|s| s + target_text.len());

            collect_expr_mutations(&aug.value, source, cursor, mutations, ignored);

            // AugAssign operator swap (e.g. += → -=)
            if let Some(op_s) = op_start {
                add_augop_mutation_at(&aug.operator, &op_text, op_s, mutations);
            }
            // AugAssign → plain Assign (e.g. a += b → a = b)
            if let Some(start) = aug_start {
                add_augassign_to_assign_at(aug, &full_text, start, mutations);
            }
        }
        SmallStatement::Assert(a) => {
            collect_expr_mutations(&a.test, source, cursor, mutations, ignored);
        }
        _ => {}
    }
}

/// Collect mutations from an expression.
///
/// The cursor is a monotonically-advancing position tracker:
///   - On entry, `*cursor` is at or before the start of `expr` in `source`.
///   - This function finds the exact start of `expr` by searching forward from `*cursor`.
///   - Children are processed with a local sub-cursor anchored at `expr_start`.
///   - On return, `*cursor` is advanced to `expr_start + expr_text.len()`.
///
/// Because the cursor only moves forward and each call anchors search to `expr_start`,
/// duplicate tokens (e.g. two `+` operators in `a + b + c`) are always found at their
/// correct respective positions.
#[allow(clippy::only_used_in_recursion)]
fn collect_expr_mutations(
    expr: &Expression,
    source: &str,
    cursor: &mut usize,
    mutations: &mut Vec<Mutation>,
    ignored: &std::collections::HashSet<usize>,
) {
    let expr_text = codegen_node(expr);

    // Find the start of this expression by searching forward from the current cursor.
    // Falls back to cursor if not found (shouldn't happen for well-formed Python).
    let expr_start = source[*cursor..]
        .find(&expr_text)
        .map(|pos| *cursor + pos)
        .unwrap_or(*cursor);

    // Local cursor anchored at expr_start; used to find children in left-to-right order.
    let mut local = expr_start;

    match expr {
        Expression::BinaryOperation(binop) => {
            // Process left; local advances to op_start.
            collect_expr_mutations(&binop.left, source, &mut local, mutations, ignored);
            // Operator starts where the left child ended.
            let op_text = codegen_node(&binop.operator);
            add_binop_mutation_at(&binop.operator, &op_text, local, mutations);
            local += op_text.len();
            collect_expr_mutations(&binop.right, source, &mut local, mutations, ignored);
        }
        Expression::BooleanOperation(boolop) => {
            collect_expr_mutations(&boolop.left, source, &mut local, mutations, ignored);
            let op_text = codegen_node(&boolop.operator);
            add_boolop_mutation_at(&boolop.operator, &op_text, local, mutations);
            local += op_text.len();
            collect_expr_mutations(&boolop.right, source, &mut local, mutations, ignored);
        }
        Expression::UnaryOperation(unop) => {
            // Record mutation on the whole unary expression before recursing.
            add_unaryop_mutation_at(unop, &expr_text, expr_start, mutations);
            collect_expr_mutations(&unop.expression, source, &mut local, mutations, ignored);
        }
        Expression::Comparison(cmp) => {
            collect_expr_mutations(&cmp.left, source, &mut local, mutations, ignored);
            for target in &cmp.comparisons {
                let op_text = codegen_node(&target.operator);
                add_compop_mutation_at(&target.operator, &op_text, local, mutations);
                local += op_text.len();
                collect_expr_mutations(&target.comparator, source, &mut local, mutations, ignored);
            }
        }
        Expression::Name(name) => {
            add_name_mutation_at(name, expr_start, mutations);
        }
        Expression::Integer(int) => {
            add_number_mutation_at(int, expr_start, mutations);
        }
        Expression::Float(float) => {
            add_float_mutation_at(float, expr_start, mutations);
        }
        Expression::SimpleString(s) => {
            add_string_mutation_at(s, expr_start, mutations);
        }
        Expression::Call(call) => {
            add_method_mutations(call, expr_start, mutations);
            collect_expr_mutations(&call.func, source, &mut local, mutations, ignored);
            for arg in &call.args {
                collect_expr_mutations(&arg.value, source, &mut local, mutations, ignored);
            }
        }
        Expression::IfExp(ifexp) => {
            // Source order: body "if" test "else" orelse
            collect_expr_mutations(&ifexp.body, source, &mut local, mutations, ignored);
            collect_expr_mutations(&ifexp.test, source, &mut local, mutations, ignored);
            collect_expr_mutations(&ifexp.orelse, source, &mut local, mutations, ignored);
        }
        Expression::Lambda(lam) => {
            add_lambda_mutation_at(lam, &expr_text, expr_start, mutations);
        }
        Expression::Tuple(t) => {
            for el in &t.elements {
                if let cst::Element::Simple {
                    value: ref e_val, ..
                } = el
                {
                    collect_expr_mutations(e_val, source, &mut local, mutations, ignored);
                }
            }
        }
        Expression::List(l) => {
            for el in &l.elements {
                if let cst::Element::Simple {
                    value: ref e_val, ..
                } = el
                {
                    collect_expr_mutations(e_val, source, &mut local, mutations, ignored);
                }
            }
        }
        Expression::Dict(d) => {
            for el in &d.elements {
                if let cst::DictElement::Simple {
                    ref key, ref value, ..
                } = el
                {
                    collect_expr_mutations(key, source, &mut local, mutations, ignored);
                    collect_expr_mutations(value, source, &mut local, mutations, ignored);
                }
            }
        }
        Expression::Subscript(sub) => {
            collect_expr_mutations(&sub.value, source, &mut local, mutations, ignored);
        }
        Expression::Attribute(attr) => {
            collect_expr_mutations(&attr.value, source, &mut local, mutations, ignored);
        }
        _ => {}
    }

    // Advance the outer cursor past this entire expression.
    *cursor = expr_start + expr_text.len();
}

// --- Operator mutation helpers (all take explicit start position) ---

/// Record a mutation at a known byte offset.
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

fn add_binop_mutation_at(
    op: &BinaryOp,
    op_text: &str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let op_trimmed = op_text.trim();
    for &(from, to) in BINOP_SWAPS {
        if op_trimmed == from {
            let replacement = op_text.replace(from, to);
            record_mutation(op_text, &replacement, "binop_swap", start, mutations);
            break;
        }
    }
    // Suppress unused warning: op is used implicitly via op_text
    let _ = op;
}

fn add_boolop_mutation_at(
    op: &BooleanOp,
    op_text: &str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let op_trimmed = op_text.trim();
    let replacement = match op_trimmed {
        "and" => op_text.replace("and", "or"),
        "or" => op_text.replace("or", "and"),
        _ => return,
    };
    record_mutation(op_text, &replacement, "boolop_swap", start, mutations);
    let _ = op;
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

fn add_compop_mutation_at(
    op: &CompOp,
    op_text: &str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let op_trimmed = op_text.trim();
    for &(from, to) in COMPOP_SWAPS {
        if op_trimmed == from.trim() {
            let replacement = op_text.replace(from.trim(), to.trim());
            record_mutation(op_text, &replacement, "compop_swap", start, mutations);
            break;
        }
    }
    let _ = op;
}

fn add_unaryop_mutation_at(
    unop: &cst::UnaryOperation,
    full_text: &str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    // not x → x, ~x → x
    match &unop.operator {
        UnaryOp::Not { .. } | UnaryOp::BitInvert { .. } => {
            let inner_text = codegen_node(&*unop.expression);
            record_mutation(full_text, &inner_text, "unary_removal", start, mutations);
        }
        _ => {}
    }
}

fn add_name_mutation_at(name: &cst::Name, start: usize, mutations: &mut Vec<Mutation>) {
    let text = name.value;
    let replacement = match text {
        "True" => "False",
        "False" => "True",
        "deepcopy" => "copy",
        _ => return,
    };
    record_mutation(text, replacement, "name_swap", start, mutations);
}

fn add_number_mutation_at(int: &cst::Integer, start: usize, mutations: &mut Vec<Mutation>) {
    let text = int.value;
    if let Ok(n) = text.replace('_', "").parse::<i64>() {
        let replacement = (n + 1).to_string();
        if replacement != text {
            record_mutation(text, &replacement, "number_mutation", start, mutations);
        }
    }
}

fn add_float_mutation_at(float: &cst::Float, start: usize, mutations: &mut Vec<Mutation>) {
    let text = float.value;
    if let Ok(n) = text.parse::<f64>() {
        let replacement = format!("{}", n + 1.0);
        if replacement != text {
            record_mutation(text, &replacement, "number_mutation", start, mutations);
        }
    }
}

fn add_string_mutation_at(s: &cst::SimpleString, start: usize, mutations: &mut Vec<Mutation>) {
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
        record_mutation(text, &replacement, "string_mutation", start, mutations);
    }
}

fn add_lambda_mutation_at(
    lam: &cst::Lambda,
    full_text: &str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let body_text = codegen_node(&*lam.body);
    let replacement_body = if body_text.trim() == "None" {
        "0"
    } else {
        "None"
    };
    let replacement = full_text.replace(&body_text, replacement_body);
    record_mutation(full_text, &replacement, "lambda_mutation", start, mutations);
}

fn add_assignment_mutation_at(
    assign: &cst::Assign,
    assign_text: &str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let value_text = codegen_node(&assign.value);
    let replacement = if value_text.trim() == "None" {
        " \"\"".to_string()
    } else {
        " None".to_string()
    };
    // Replace just the value portion: find the `=` and substitute everything after it.
    let new_full = if let Some(eq_pos) = assign_text.find('=') {
        format!("{}={replacement}", &assign_text[..eq_pos])
    } else {
        return;
    };
    record_mutation(assign_text, &new_full, "assignment_mutation", start, mutations);
}

/// AugAssign operator swap: += → -=, etc.
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

fn add_augop_mutation_at(
    op: &cst::AugOp,
    op_text: &str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let op_trimmed = op_text.trim();
    for &(from, to) in AUGOP_SWAPS {
        if op_trimmed == from {
            let replacement = op_text.replace(from, to);
            record_mutation(op_text, &replacement, "augop_swap", start, mutations);
            break;
        }
    }
    let _ = op;
}

fn add_augassign_to_assign_at(
    aug: &cst::AugAssign,
    full_text: &str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let target_text = codegen_node(&aug.target);
    let value_text = codegen_node(&aug.value);
    let plain_assign = format!("{target_text} ={value_text}");
    record_mutation(full_text, &plain_assign, "augassign_to_assign", start, mutations);
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
    expr_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    if let Expression::Attribute(attr) = &*call.func {
        let method_text = codegen_node(&attr.attr);
        let method_trimmed = method_text.trim();

        for &(from, to) in METHOD_SWAPS {
            if method_trimmed == from {
                let func_text = codegen_node(&call.func);
                if let Some(pos) = func_text.rfind(from) {
                    record_mutation(from, to, "method_swap", expr_start + pos, mutations);
                }
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
        &func_source[..mutation.start],
        mutation.replacement,
        &func_source[mutation.end..]
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

    // INV-1: a + b + c produces 2 independent mutations, each applied correctly
    #[test]
    fn test_duplicate_operators_independent_mutations() {
        let source = "def foo(a, b, c):\n    return a + b + c\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];

        let binops: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();
        assert_eq!(binops.len(), 2, "Should find exactly 2 + operators");

        // They must be at different positions
        assert_ne!(
            binops[0].start, binops[1].start,
            "Duplicate operators must be at distinct positions"
        );

        // Applying each mutation should produce distinct correct outputs
        let mutated0 = apply_mutation(&fm.source, binops[0]);
        let mutated1 = apply_mutation(&fm.source, binops[1]);

        // One mutation: a - b + c, Other: a + b - c
        let has_a_minus = mutated0.contains("a - b + c") || mutated1.contains("a - b + c");
        let has_b_minus = mutated0.contains("a + b - c") || mutated1.contains("a + b - c");
        assert!(has_a_minus, "One mutant should be 'a - b + c', got: {mutated0} and {mutated1}");
        assert!(has_b_minus, "One mutant should be 'a + b - c', got: {mutated0} and {mutated1}");
    }

    // INV-2: Applying mutation N produces exactly the expected output (no off-by-one)
    #[test]
    fn test_apply_mutation_exact_positions() {
        // Without spaces: a+b
        let source = "def foo(a, b):\n    return a+b\n";
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        let binop = fm
            .mutations
            .iter()
            .find(|m| m.operator == "binop_swap")
            .unwrap();

        // original should be exactly "+"
        assert_eq!(binop.original, "+", "Operator without spaces");
        let mutated = apply_mutation(&fm.source, binop);
        assert!(mutated.contains("a-b"), "Should produce a-b, got: {mutated}");
        assert!(!mutated.contains("a+b"), "Original + should be gone");
    }

    // INV-3: Nested operators at correct positions
    #[test]
    fn test_nested_operators() {
        let source = "def foo(a, b, c, d):\n    return (a + b) * (c + d)\n";
        let fms = collect_file_mutations(source);
        let fm = &fms[0];

        let binops: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();

        // Should have 3 operators: +, *, +
        assert_eq!(binops.len(), 3, "Should find 3 operators: +, *, +");

        // All at different positions
        let positions: std::collections::HashSet<usize> =
            binops.iter().map(|m| m.start).collect();
        assert_eq!(positions.len(), 3, "All operators must be at distinct positions");

        // Each mutation should produce syntactically reasonable output
        for m in &binops {
            let mutated = apply_mutation(&fm.source, m);
            // The mutated source should still contain def and return
            assert!(mutated.contains("def foo"), "Mutated source should still have def");
            assert!(mutated.contains("return"), "Mutated source should still have return");
        }
    }

    // Mixed case: x = a + a
    #[test]
    fn test_duplicate_operand_mutation() {
        let source = "def foo(a):\n    x = a + a\n";
        let fms = collect_file_mutations(source);
        let fm = &fms[0];

        let binops: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();
        assert_eq!(binops.len(), 1, "Should find exactly 1 + operator");

        let mutated = apply_mutation(&fm.source, binops[0]);
        assert!(mutated.contains("a - a"), "Should produce a - a, got: {mutated}");
    }

    // Byte-span correctness: start and end span exactly the original text
    #[test]
    fn test_mutation_span_correctness() {
        let source = "def foo(a, b, c):\n    return a + b + c\n";
        let fms = collect_file_mutations(source);
        let fm = &fms[0];

        for m in &fm.mutations {
            let slice = &fm.source[m.start..m.end];
            assert_eq!(
                slice, m.original,
                "Span [{}, {}) should equal original '{}'",
                m.start, m.end, m.original
            );
        }
    }
}
