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
    /// Return type annotation text, e.g. " -> int | None". Empty if none.
    pub return_annotation: String,
    /// Whether the function is async.
    pub is_async: bool,
    /// Whether the function is a generator (contains `yield` at the function body level,
    /// not inside nested functions). An async generator has both `is_async` and `is_generator`.
    pub is_generator: bool,
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

/// Builtin function calls that are never mutated (neither the call itself nor its arguments).
static NEVER_MUTATE_FUNCTION_CALLS: &[&str] = &["len", "isinstance"];

fn collect_function_mutations(
    func: &cst::FunctionDef,
    class_name: Option<&str>,
    full_source: &str,
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
    let is_generator = suite_contains_yield(&func.body);

    // Extract return type annotation, e.g. " -> int | None"
    let return_annotation = if let Some(ann) = &func.returns {
        let mut state = CodegenState::default();
        ann.codegen(&mut state, "->");
        state.tokens
    } else {
        String::new()
    };

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

    // Post-collection pragma filtering: map each mutation's byte offset within
    // func_source to an absolute line number in the full file, then drop any
    // mutation whose line is annotated with `# pragma: no mutate`.
    if !ignored_lines.is_empty() {
        // Find where this function starts in the full source so we can translate
        // func-local line numbers to file-level line numbers.
        let func_start_line = full_source
            .find(&func_source)
            .map(|byte_off| offset_to_line(full_source, byte_off))
            .unwrap_or(1);

        mutations.retain(|m| {
            let line_in_func = offset_to_line(&func_source, m.start);
            // func_start_line is the line of `def …:` (1-indexed); add the
            // within-function line offset (also 1-indexed), subtract 1 to avoid
            // double-counting the base.
            let line_in_file = func_start_line + line_in_func - 1;
            !ignored_lines.contains(&line_in_file)
        });
    }

    if mutations.is_empty() {
        return None;
    }

    Some(FunctionMutations {
        name,
        class_name: class_name.map(String::from),
        source: func_source,
        params_source,
        return_annotation,
        is_async,
        is_generator,
        mutations,
    })
}

// --- Generator detection ---
//
// A function is a generator if its body contains `yield` at the function's own
// scope level (not inside nested function definitions).

fn suite_contains_yield(suite: &cst::Suite) -> bool {
    match suite {
        cst::Suite::IndentedBlock(block) => block.body.iter().any(stmt_contains_yield),
        cst::Suite::SimpleStatementSuite(s) => s.body.iter().any(small_stmt_contains_yield),
    }
}

fn stmt_contains_yield(stmt: &Statement) -> bool {
    match stmt {
        Statement::Simple(simple) => simple.body.iter().any(small_stmt_contains_yield),
        Statement::Compound(compound) => match compound {
            // Do NOT recurse into nested functions — yield there does not make the
            // outer function a generator.
            CompoundStatement::FunctionDef(_) => false,
            CompoundStatement::If(if_stmt) => {
                expr_contains_yield(&if_stmt.test)
                    || suite_contains_yield(&if_stmt.body)
                    || if_stmt
                        .orelse
                        .as_ref()
                        .is_some_and(|orelse| match orelse.as_ref() {
                            cst::OrElse::Elif(elif) => {
                                expr_contains_yield(&elif.test) || suite_contains_yield(&elif.body)
                            }
                            cst::OrElse::Else(else_clause) => {
                                suite_contains_yield(&else_clause.body)
                            }
                        })
            }
            CompoundStatement::While(w) => {
                expr_contains_yield(&w.test) || suite_contains_yield(&w.body)
            }
            CompoundStatement::For(f) => {
                expr_contains_yield(&f.iter) || suite_contains_yield(&f.body)
            }
            CompoundStatement::With(w) => {
                w.items.iter().any(|item| expr_contains_yield(&item.item))
                    || suite_contains_yield(&w.body)
            }
            CompoundStatement::Try(t) => {
                suite_contains_yield(&t.body)
                    || t.handlers.iter().any(|h| suite_contains_yield(&h.body))
                    || t.finalbody
                        .as_ref()
                        .is_some_and(|fin| suite_contains_yield(&fin.body))
            }
            CompoundStatement::Match(m) => m.cases.iter().any(|c| suite_contains_yield(&c.body)),
            _ => false,
        },
    }
}

fn small_stmt_contains_yield(stmt: &SmallStatement) -> bool {
    match stmt {
        SmallStatement::Return(ret) => ret.value.as_ref().is_some_and(|v| expr_contains_yield(v)),
        SmallStatement::Expr(e) => expr_contains_yield(&e.value),
        SmallStatement::Assign(a) => expr_contains_yield(&a.value),
        SmallStatement::AugAssign(aug) => expr_contains_yield(&aug.value),
        SmallStatement::Assert(a) => expr_contains_yield(&a.test),
        _ => false,
    }
}

fn expr_contains_yield(expr: &Expression) -> bool {
    match expr {
        Expression::Yield(_) => true,
        Expression::BinaryOperation(binop) => {
            expr_contains_yield(&binop.left) || expr_contains_yield(&binop.right)
        }
        Expression::BooleanOperation(boolop) => {
            expr_contains_yield(&boolop.left) || expr_contains_yield(&boolop.right)
        }
        Expression::UnaryOperation(unop) => expr_contains_yield(&unop.expression),
        Expression::Comparison(cmp) => {
            expr_contains_yield(&cmp.left)
                || cmp
                    .comparisons
                    .iter()
                    .any(|c| expr_contains_yield(&c.comparator))
        }
        Expression::Call(call) => {
            expr_contains_yield(&call.func)
                || call.args.iter().any(|a| expr_contains_yield(&a.value))
        }
        Expression::IfExp(ifexp) => {
            expr_contains_yield(&ifexp.body)
                || expr_contains_yield(&ifexp.test)
                || expr_contains_yield(&ifexp.orelse)
        }
        Expression::Tuple(t) => t.elements.iter().any(|el| {
            if let cst::Element::Simple {
                value: ref e_val, ..
            } = el
            {
                expr_contains_yield(e_val)
            } else {
                false
            }
        }),
        _ => false,
    }
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
            CompoundStatement::Match(m) => {
                // Save cursor before any recursion so we can find the match statement start.
                let cursor_before = *cursor;
                // Recurse into subject and case bodies for expression mutations.
                collect_expr_mutations(&m.subject, source, cursor, mutations, ignored);
                for case in &m.cases {
                    if let Some(ref guard) = case.guard {
                        collect_expr_mutations(guard, source, cursor, mutations, ignored);
                    }
                    collect_suite_mutations(&case.body, source, cursor, mutations, ignored);
                }
                // Generate match case removal mutations (one per case, when N > 1).
                add_match_case_removal_mutations(m, source, cursor_before, mutations);
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
        SmallStatement::AnnAssign(a) => {
            // Type annotations are never mutated. Only process the assigned value (if any).
            // The full AnnAssign text is "target: annotation" or "target: annotation = value".
            let full_text = codegen_node(a);
            let stmt_start = source[*cursor..]
                .find(&full_text)
                .map(|p| *cursor + p)
                .unwrap_or(*cursor);

            if let Some(ref val) = a.value {
                let val_text = codegen_node(val);
                // Use rfind so that if val_text appears in the annotation too, we find the
                // value occurrence (which is always last in the full text).
                let val_in_full = full_text
                    .rfind(&val_text)
                    .unwrap_or(full_text.len().saturating_sub(val_text.len()));
                *cursor = stmt_start + val_in_full;
                collect_expr_mutations(val, source, cursor, mutations, ignored);
            } else {
                // Pure annotation (no value): "x: int" — advance cursor past it entirely.
                *cursor = stmt_start + full_text.len();
            }
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
            // Skip calls to builtins that should never be mutated (len, isinstance, etc.).
            let is_skip = matches!(&*call.func, Expression::Name(n) if NEVER_MUTATE_FUNCTION_CALLS.contains(&n.value));
            if !is_skip {
                add_method_mutations(call, expr_start, mutations);
                add_arg_removal_mutations(call, &expr_text, expr_start, mutations);
                collect_expr_mutations(&call.func, source, &mut local, mutations, ignored);
                for arg in &call.args {
                    collect_expr_mutations(&arg.value, source, &mut local, mutations, ignored);
                }
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

fn add_compop_mutation_at(op: &CompOp, op_text: &str, start: usize, mutations: &mut Vec<Mutation>) {
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

    // Find the actual delimiter: the first quote character in the literal text.
    // Strings may have prefixes (r, b, f, rb, etc.) before the opening quote.
    // The old code used `text.contains('"')` which would incorrectly pick '"'
    // as the delimiter for '"' (a single-quoted string containing a double-quote),
    // producing invalid Python like '"XXXX" (unterminated single-quoted string).
    let prefix_end = match text.find(['"', '\'']) {
        Some(idx) => idx,
        None => return, // malformed, skip
    };
    let quote_char = text.as_bytes()[prefix_end] as char;

    let prefix = &text[..prefix_end];
    let inner = &text[prefix_end + 1..text.len() - 1];

    // INV-2: If the inner content contains the actual delimiter character, skip.
    // This can happen with escaped content like "it\"s" where inner = r#"it\"s"#.
    // The mutation would still be valid Python (the backslash stays), but more
    // importantly: if inner is exactly the delimiter char (e.g. '"' → inner = '"'),
    // after the quote_char fix above the generated replacement is 'XX"XX' which IS
    // valid Python. This guard is belt-and-suspenders for any other edge cases.
    if inner.contains(quote_char) {
        return;
    }

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
    // Use a structural byte-offset splice instead of String::replace(), which would replace
    // ALL occurrences of body_text in full_text — including any that appear in param names.
    // Lambda params can never contain `:`, so the first `:` in the lambda text is always the
    // colon separator between params and body.
    let colon_pos = match full_text.find(':') {
        Some(p) => p,
        None => return, // malformed lambda; skip
    };
    let after_colon = &full_text[colon_pos + 1..];
    let ws_len = after_colon.find(|c: char| !c.is_whitespace()).unwrap_or(0);
    let body_start = colon_pos + 1 + ws_len;
    let body_end = body_start + body_text.len();
    if body_end > full_text.len() {
        return; // safety guard: malformed body offset
    }
    let replacement = format!(
        "{}{}{}",
        &full_text[..body_start],
        replacement_body,
        &full_text[body_end..]
    );
    record_mutation(full_text, &replacement, "lambda_mutation", start, mutations);
}

fn add_assignment_mutation_at(
    assign: &cst::Assign,
    assign_text: &str,
    start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let value_text = codegen_node(&assign.value);
    let replacement = if value_text.trim() == "None" { "\"\"" } else { "None" };
    // Find the start of the value by summing the codegen lengths of all AssignTarget nodes.
    // Each AssignTarget codegen is: {target}{whitespace_before_equal}={whitespace_after_equal},
    // so the value always begins immediately after the last target. This is safer than
    // assign_text.find('='), which returns the wrong position for chained assignments like
    // `a = b = c` (would match the first `=`, silently dropping `b` as a target), and also
    // mismatches `=` inside string literals (`d['=']`) or `==` comparisons in the value.
    let targets_len: usize = assign.targets.iter().map(|t| codegen_node(t).len()).sum();
    if targets_len > assign_text.len() {
        return;
    }
    let new_full = format!("{}{}", &assign_text[..targets_len], replacement);
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
    record_mutation(
        full_text,
        &plain_assign,
        "augassign_to_assign",
        start,
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
    ("ljust", "rjust"),
    ("rjust", "ljust"),
    ("index", "rindex"),
    ("rindex", "index"),
    ("removeprefix", "removesuffix"),
    ("removesuffix", "removeprefix"),
    ("partition", "rpartition"),
    ("rpartition", "partition"),
];

fn add_method_mutations(call: &cst::Call, expr_start: usize, mutations: &mut Vec<Mutation>) {
    if let Expression::Attribute(attr) = &*call.func {
        let method_text = codegen_node(&attr.attr);
        let method_trimmed = method_text.trim();

        for &(from, to) in METHOD_SWAPS {
            if method_trimmed == from {
                let func_text = codegen_node(&*call.func);
                // Structural offset: the method name is always after the last dot in an
                // Attribute node. Using rfind('.') is a structural guarantee, unlike
                // rfind(method_name) which is a text heuristic that happens to work for
                // symmetric cases like `find.find()` but is not structurally sound.
                let dot_pos = func_text.rfind('.').expect("Attribute node always contains a dot");
                // Skip any whitespace between the dot and the method name (codegen may add space).
                let after_dot = &func_text[dot_pos + 1..];
                let leading_ws = after_dot.len() - after_dot.trim_start().len();
                let method_start = dot_pos + 1 + leading_ws;
                record_mutation(from, to, "method_swap", expr_start + method_start, mutations);
                break;
            }
        }
    }
}

/// Build the text of an `Arg` for use in reconstructed call expressions, omitting any
/// trailing comma (so that callers can join with `", "` freely).
fn arg_text_no_comma(arg: &cst::Arg) -> String {
    let star = arg.star;
    let kw_part = if let Some(ref kw) = arg.keyword {
        format!("{}=", kw.value)
    } else {
        String::new()
    };
    let value = codegen_node(&arg.value);
    format!("{star}{kw_part}{value}")
}

/// Generate arg-removal mutations for a function call expression.
///
/// For each argument that is not a starred (`*`/`**`) expression:
/// 1. If the argument is not already `None`, generate a mutation that replaces it with `None`.
/// 2. If the call has more than one argument, generate a mutation that removes the argument
///    entirely (with its surrounding comma handled implicitly by reconstructing the arg list).
///
/// Both mutation kinds use the operator name `"arg_removal"`.
fn add_arg_removal_mutations(
    call: &cst::Call,
    call_text: &str,
    expr_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let args = &call.args;
    if args.is_empty() {
        return;
    }

    let func_text = codegen_node(&*call.func);

    for (i, arg) in args.iter().enumerate() {
        // Skip *args and **kwargs (starred expressions).
        if !arg.star.is_empty() {
            continue;
        }

        let arg_value_text = codegen_node(&arg.value);
        let is_none = arg_value_text.trim() == "None";

        // Mutation 1: replace this arg's value with None (skip if already None).
        if !is_none {
            let new_args: Vec<String> = args
                .iter()
                .enumerate()
                .map(|(j, a)| {
                    if j == i {
                        // Preserve keyword= prefix, replace value with None.
                        if let Some(ref kw) = a.keyword {
                            format!("{}=None", kw.value)
                        } else {
                            "None".to_string()
                        }
                    } else {
                        arg_text_no_comma(a)
                    }
                })
                .collect();
            let new_call = format!("{}({})", func_text, new_args.join(", "));
            record_mutation(call_text, &new_call, "arg_removal", expr_start, mutations);
        }

        // Mutation 2: remove this arg entirely (skip if it is the only arg).
        // Comma handling is implicit: we reconstruct the arg list without the removed arg
        // and join with ", ", which correctly handles first/middle/last removal.
        if args.len() > 1 {
            let new_args: Vec<String> = args
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, a)| arg_text_no_comma(a))
                .collect();
            let new_call = format!("{}({})", func_text, new_args.join(", "));
            record_mutation(call_text, &new_call, "arg_removal", expr_start, mutations);
        }
    }
}

// --- Match case removal ---

/// Generate one mutation per case in a match statement, each removing one case block.
///
/// Strategy: locate the match keyword in func_source using text search from cursor_before,
/// then locate each case using its CST-derived pattern text as a search anchor, advancing
/// a sub-cursor through the match body. This is consistent with the cursor-based approach
/// used by all other operators and eliminates the indentation-scanning approach.
///
/// Note: libcst codegen uses relative indentation (`state.add_indent()`), so calling
/// `codegen_node(match_stmt)` standalone would strip the leading indent.  We therefore
/// keep the subject-text search for the match header and use pattern-text anchors for
/// individual cases, both of which are indent-independent.
fn add_match_case_removal_mutations(
    match_stmt: &cst::Match,
    source: &str,
    cursor_before: usize,
    mutations: &mut Vec<Mutation>,
) {
    let n_cases = match_stmt.cases.len();
    if n_cases <= 1 {
        return;
    }

    // Find the "match <subject>:" header in source from cursor_before.
    let subject_text = codegen_node(&match_stmt.subject);
    let match_header_pattern = format!("match {subject_text}:");
    let match_kw_pos = match source[cursor_before..].find(&match_header_pattern) {
        Some(p) => cursor_before + p,
        None => return,
    };

    // Compute the match line start (byte offset of the indent before "match").
    let match_line_start = source[..match_kw_pos].rfind('\n').map_or(0, |p| p + 1);
    let match_indent_len = match_kw_pos - match_line_start;

    // Find the end of the match header line (the \n after "match ...:").
    let match_header_end = match source[match_kw_pos..].find('\n') {
        Some(p) => match_kw_pos + p + 1,
        None => return,
    };

    // Find where the match block ends (first non-blank line at <= match_indent_len, or source end).
    let match_end = find_block_end(source, match_header_end, match_indent_len);

    // Locate each case block start using the CST pattern text as the search anchor.
    let case_line_starts = match find_case_starts_by_pattern(
        source,
        match_header_end,
        match_end,
        &match_stmt.cases,
    ) {
        Some(v) => v,
        None => return,
    };

    // The full original text of the match statement (including its leading indentation).
    let match_original = &source[match_line_start..match_end];

    // Generate one removal mutation per case.
    for i in 0..n_cases {
        let case_rel_start = case_line_starts[i] - match_line_start;
        let case_rel_end = if i + 1 < n_cases {
            case_line_starts[i + 1] - match_line_start
        } else {
            match_end - match_line_start
        };

        let replacement = format!(
            "{}{}",
            &match_original[..case_rel_start],
            &match_original[case_rel_end..]
        );

        record_mutation(
            match_original,
            &replacement,
            "match_case_removal",
            match_line_start,
            mutations,
        );
    }
}

/// Find the line-start byte offsets of each case in a match statement body,
/// using CST-derived case header text as the search anchor.
///
/// For each case, constructs the exact `case <pattern>:` (or `case <pattern> if <guard>:`)
/// header text using the whitespace values stored in the CST nodes, then searches forward
/// from a sub-cursor within `[from, match_end)`.
///
/// Candidate matches are validated by checking that only whitespace precedes the `case`
/// keyword on that line — this skips false matches inside string literals or comments.
///
/// Returns `None` if any case cannot be located.
fn find_case_starts_by_pattern<'a>(
    source: &str,
    from: usize,
    match_end: usize,
    cases: &[cst::MatchCase<'a>],
) -> Option<Vec<usize>> {
    let mut result = Vec::new();
    let mut sub_cursor = from;

    for case in cases {
        // Build the exact case header anchor from CST-stored whitespace fields.
        // SimpleWhitespace is a newtype wrapper: `.0` gives the raw &str.
        let ws = case.whitespace_after_case.0;
        let pattern_text = codegen_node(&case.pattern);
        let ws_bc = case.whitespace_before_colon.0;
        let case_anchor = if let Some(guard) = &case.guard {
            let ws_bi = case.whitespace_before_if.0;
            let ws_ai = case.whitespace_after_if.0;
            let guard_text = codegen_node(guard);
            format!("case{ws}{pattern_text}{ws_bi}if{ws_ai}{guard_text}{ws_bc}:")
        } else {
            format!("case{ws}{pattern_text}{ws_bc}:")
        };

        // Search for the anchor, skipping any false match not at a line start
        // (e.g. the anchor text appearing inside a string literal or comment).
        let mut search_from = sub_cursor;
        let case_line_start = loop {
            let needle_pos = source[search_from..match_end].find(&case_anchor)?;
            let abs_needle = search_from + needle_pos;
            let line_start = source[..abs_needle].rfind('\n').map_or(0, |p| p + 1);
            // Validate: only whitespace between the line start and the "case" keyword.
            let prefix = &source[line_start..abs_needle];
            if prefix.chars().all(|c| c == ' ' || c == '\t') {
                sub_cursor = abs_needle + case_anchor.len();
                break line_start;
            }
            // False match — skip past it and retry.
            search_from = abs_needle + 1;
        };

        result.push(case_line_start);
    }

    Some(result)
}

/// Find the byte offset where a block indented deeper than `block_indent_len` ends.
///
/// Starting from `from`, scans lines until it finds a non-blank line at indentation
/// ≤ `block_indent_len` (which signals the end of the block) or reaches the end of source.
fn find_block_end(source: &str, from: usize, block_indent_len: usize) -> usize {
    let mut pos = from;

    while pos < source.len() {
        let line_end = source[pos..]
            .find('\n')
            .map_or(source.len(), |p| pos + p + 1);
        let line = &source[pos..line_end];

        if !line.trim().is_empty() {
            let line_indent = line.len() - line.trim_start_matches([' ', '\t']).len();
            if line_indent <= block_indent_len {
                return pos;
            }
        }

        pos = line_end;
    }

    source.len()
}

// --- Utility ---

fn codegen_node<'a>(node: &impl Codegen<'a>) -> String {
    let mut state = CodegenState::default();
    node.codegen(&mut state);
    state.tokens
}

/// Return the 1-indexed line number of `offset` within `text`.
fn offset_to_line(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
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
        // Entire body is on a pragma line — all mutations suppressed, function omitted.
        let source = "def foo():\n    return 1 + 2  # pragma: no mutate\n";
        let fms = collect_file_mutations(source);
        assert!(
            fms.is_empty(),
            "All mutations suppressed → function should be omitted"
        );
    }

    #[test]
    fn test_pragma_blocks_binop() {
        let source = "def foo(a, b):\n    return a + b  # pragma: no mutate\n";
        let fms = collect_file_mutations(source);
        let binops: Vec<_> = fms
            .first()
            .map(|f| {
                f.mutations
                    .iter()
                    .filter(|m| m.operator == "binop_swap")
                    .collect()
            })
            .unwrap_or_default();
        assert!(binops.is_empty(), "Pragma should block + → - mutation");
    }

    #[test]
    fn test_pragma_selective() {
        // Line 3 uses `*` (unique token on pragma line) to avoid cursor-offset ambiguity.
        // Lines 2 and 4 have `+` and should each produce one binop mutation.
        let source =
            "def foo(a, b, c):\n    x = a + b\n    y = b * c  # pragma: no mutate\n    return x + y\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "Function should still be collected");
        let binops: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "binop_swap")
            .collect();
        // Lines 2 (+) and 4 (+) produce mutations; line 3 (*) is suppressed entirely.
        assert_eq!(
            binops.len(),
            2,
            "Should have mutations for lines 2 and 4, but not the pragma line 3"
        );
    }

    #[test]
    fn test_pragma_whole_line_all_operators() {
        // A line with both a binop and a comparison — pragma suppresses all of them.
        let source = "def foo(a, b):\n    x = 1 + 2  # pragma: no mutate\n    return a > b\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "Non-pragma line still produces mutations");
        // Number/binop mutations from line 2 should be gone; compop from line 3 remains.
        let line2_muts: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "number_mutation" || m.operator == "binop_swap")
            .collect();
        assert!(
            line2_muts.is_empty(),
            "Pragma suppresses all operators on that line"
        );
        let compop = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "compop_swap");
        assert!(compop.is_some(), "Non-pragma lines are unaffected");
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
        let method_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap");
        assert!(
            method_mut.is_some(),
            "Should find .lower() → .upper() mutation"
        );
        let m = method_mut.unwrap();
        assert_eq!(m.original, "lower");
        assert_eq!(m.replacement, "upper");
    }

    #[test]
    fn test_method_swap_lstrip_rstrip() {
        let source = "def foo(s):\n    return s.lstrip()\n";
        let fms = collect_file_mutations(source);
        let method_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap");
        assert!(method_mut.is_some());
        let m = method_mut.unwrap();
        assert_eq!(m.original, "lstrip");
        assert_eq!(m.replacement, "rstrip");
    }

    #[test]
    fn test_method_swap_ljust_rjust() {
        let source = "def foo(s):\n    return s.ljust(10)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "ljust");
        assert_eq!(m.replacement, "rjust");
    }

    #[test]
    fn test_method_swap_rjust_ljust() {
        let source = "def foo(s):\n    return s.rjust(10)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "rjust");
        assert_eq!(m.replacement, "ljust");
    }

    #[test]
    fn test_method_swap_index_rindex() {
        let source = "def foo(s):\n    return s.index('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "index");
        assert_eq!(m.replacement, "rindex");
    }

    #[test]
    fn test_method_swap_rindex_index() {
        let source = "def foo(s):\n    return s.rindex('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "rindex");
        assert_eq!(m.replacement, "index");
    }

    #[test]
    fn test_method_swap_removeprefix_removesuffix() {
        let source = "def foo(s):\n    return s.removeprefix('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "removeprefix");
        assert_eq!(m.replacement, "removesuffix");
    }

    #[test]
    fn test_method_swap_removesuffix_removeprefix() {
        let source = "def foo(s):\n    return s.removesuffix('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "removesuffix");
        assert_eq!(m.replacement, "removeprefix");
    }

    #[test]
    fn test_method_swap_partition_rpartition() {
        let source = "def foo(s):\n    return s.partition('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "partition");
        assert_eq!(m.replacement, "rpartition");
    }

    #[test]
    fn test_method_swap_rpartition_partition() {
        let source = "def foo(s):\n    return s.rpartition('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .unwrap();
        assert_eq!(m.original, "rpartition");
        assert_eq!(m.replacement, "partition");
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

    // INV-1: When the object variable name equals the method name, the mutation span
    // must cover the method (after the dot), NOT the object name (before the dot).
    #[test]
    fn test_method_swap_object_name_equals_method_name() {
        let source = "def foo(s):\n    return find.find('x')\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("Should find method_swap mutation on find.find()");

        assert_eq!(m.original, "find");
        assert_eq!(m.replacement, "rfind");

        // The span must cover exactly the method name text.
        let span_text = &fms[0].source[m.start..m.end];
        assert_eq!(span_text, "find", "Span should cover the method name, not the object");

        // The character immediately before the method start must be a dot.
        assert_eq!(
            &fms[0].source[m.start - 1..m.start],
            ".",
            "Character before method span start must be a dot"
        );
    }

    // INV-3: For any method_swap mutation m, source[m.start..m.end] equals the original name.
    // Also validates that the character before the span is always a dot (structural guarantee).
    #[test]
    fn test_method_swap_span_structural_correctness() {
        let cases = [
            "def foo(s):\n    return s.lower()\n",
            "def foo(s):\n    return s.upper()\n",
            "def foo(s):\n    return find.find('x')\n",
            "def foo(s):\n    return lower.lower()\n",
            "def foo(s):\n    return s.strip().lower()\n",
            "def foo(s):\n    return upper.upper()\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in &fm.mutations {
                    if m.operator == "method_swap" {
                        let span_text = &fm.source[m.start..m.end];
                        assert_eq!(
                            span_text, m.original,
                            "INV-3: span [{}, {}) = {:?} but original = {:?} in {:?}",
                            m.start, m.end, span_text, m.original, source
                        );
                        // Structural guarantee: immediately before the method name is always a dot.
                        assert_eq!(
                            &fm.source[m.start - 1..m.start],
                            ".",
                            "Character before method span must be a dot, violated in {:?}",
                            source
                        );
                    }
                }
            }
        }
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

    // INV-2: string content = delimiter char must not produce syntactically invalid Python
    #[test]
    fn test_string_mutation_double_quote_in_single_quoted() {
        // '"' is a single-quoted string whose content is a double-quote.
        // Before the fix, quote_char was incorrectly detected as '"', producing
        // the invalid replacement '"XXXX" (unterminated single-quoted string).
        // After the fix, quote_char = '\'', producing 'XX"XX' (valid Python).
        let source = "def foo(s):\n    return s.replace('\"', 'x')\n";
        let fms = collect_file_mutations(source);
        if let Some(fm) = fms.first() {
            for m in fm
                .mutations
                .iter()
                .filter(|m| m.operator == "string_mutation")
            {
                // The replacement must be a valid Python string literal.
                // For '"', it must be delimited by single-quotes: starts with ' ends with '
                if m.original == "'\"'" {
                    assert!(
                        m.replacement.starts_with('\'') && m.replacement.ends_with('\''),
                        "Replacement for '\"' must stay single-quoted, got: {}",
                        m.replacement
                    );
                    // Must not start with '"' (which would produce an unterminated string)
                    assert!(
                        !m.replacement.starts_with("'\""),
                        "Replacement must not produce unterminated string, got: {}",
                        m.replacement
                    );
                }
            }
        }
    }

    // INV-2: single-quote inside double-quoted string must also produce valid Python
    #[test]
    fn test_string_mutation_single_quote_in_double_quoted() {
        // "'" is a double-quoted string whose content is a single-quote.
        let source = "def foo(s):\n    return s.replace(\"'\", 'x')\n";
        let fms = collect_file_mutations(source);
        if let Some(fm) = fms.first() {
            for m in fm
                .mutations
                .iter()
                .filter(|m| m.operator == "string_mutation")
            {
                if m.original == "\"'\"" {
                    // Must be delimited by double-quotes
                    assert!(
                        m.replacement.starts_with('"') && m.replacement.ends_with('"'),
                        "Replacement for \"'\" must stay double-quoted, got: {}",
                        m.replacement
                    );
                }
            }
        }
    }

    // INV-3: Normal string mutations must still work after the delimiter-char fix.
    #[test]
    fn test_string_mutation_normal_strings_unaffected() {
        let source = "def greet():\n    return \"hello\"\n";
        let fms = collect_file_mutations(source);
        let string_mut = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "string_mutation");
        assert!(string_mut.is_some(), "Normal string must still be mutated");
        assert_eq!(
            string_mut.unwrap().replacement,
            "\"XXhelloXX\"",
            "Normal string mutation should produce XXhelloXX"
        );
    }

    // INV-1: Applying string mutation to a delimiter-char string must produce parseable Python.
    // Regression test for the markupsafe case: replace('"', "&#34;") where '"' is a
    // single-quoted string whose content IS the double-quote delimiter character.
    // Before the fix, the generated mutant '"XXXX" was an unterminated string → SyntaxError.
    #[test]
    fn test_string_mutation_delimiter_char_produces_parseable_python() {
        // Mirrors markupsafe's _native.py: .replace('"', "&#34;")
        let source = "def escape(s):\n    return s.replace('\"', '&#34;')\n";
        let fms = collect_file_mutations(source);
        let fm = fms.first().expect("should collect mutations from escape()");
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "string_mutation")
        {
            let mutated_func = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated_func, None).is_ok(),
                "Mutating '{}' → '{}' produced unparseable Python:\n{}",
                m.original,
                m.replacement,
                mutated_func
            );
        }
    }

    // --- Generator detection tests ---
    //
    // Note: the mutation engine only collects mutations for specific operators
    // (comparison, binop, boolop, number, string, etc.). Generator functions must
    // contain at least one such mutation to be collected. We use `if n > 0:` for
    // comparisons, which guarantees a compop mutation.

    // INV-1: A function with `yield` at the top level is a generator.
    #[test]
    fn test_generator_function_is_detected() {
        // `n > 0` produces a compop mutation, so the function is collected.
        let source = "def gen(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should find mutations (compop on n > 0)");
        assert!(
            fms[0].is_generator,
            "function with yield should be is_generator=true"
        );
        assert!(!fms[0].is_async, "plain generator is not async");
    }

    // INV-2: An async function with `yield` is an async generator.
    #[test]
    fn test_async_generator_function_is_detected() {
        let source = "async def agen(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should find mutations (compop on n > 0)");
        assert!(
            fms[0].is_generator,
            "async function with yield should be is_generator=true"
        );
        assert!(fms[0].is_async, "should also be is_async=true");
    }

    // INV-3: A regular function (no yield) is NOT a generator.
    #[test]
    fn test_regular_function_not_generator() {
        let source = "def add(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        assert!(
            !fms[0].is_generator,
            "regular function must not be is_generator"
        );
    }

    // INV-4: yield in a separate function does NOT affect `is_generator` of a different function.
    #[test]
    fn test_non_generator_function_is_not_generator() {
        // outer has only a binop mutation; inner (separate top-level) has yield + compop.
        let source =
            "def outer(x):\n    return x + 1\n\ndef inner(n):\n    if n > 0:\n        yield n\n";
        let fms = collect_file_mutations(source);
        let outer = fms
            .iter()
            .find(|fm| fm.name == "outer")
            .expect("outer must exist");
        let inner = fms
            .iter()
            .find(|fm| fm.name == "inner")
            .expect("inner must exist");
        assert!(
            !outer.is_generator,
            "outer has no yield, must not be is_generator"
        );
        assert!(inner.is_generator, "inner has yield, must be is_generator");
    }

    // --- arg_removal operator tests ---

    fn arg_removal_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "arg_removal")
            .collect()
    }

    // INV-1: f(a, b) → 4 arg_removal mutations: replace each arg + remove each arg
    #[test]
    fn test_arg_removal_two_args() {
        let source = "def foo(a, b):\n    f(a, b)\n";
        let muts = arg_removal_mutations(source);
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        assert_eq!(
            muts.len(),
            4,
            "f(a, b) must produce 4 arg_removal mutations; got: {replacements:?}"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None, b)")),
            "missing f(None, b)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, None)")),
            "missing f(a, None)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(b)")),
            "missing f(b)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a)")),
            "missing f(a)"
        );
    }

    // INV-2: f(a) → 1 mutation: replace with None (no removal)
    #[test]
    fn test_arg_removal_single_arg() {
        let source = "def foo(a):\n    f(a)\n";
        let muts = arg_removal_mutations(source);
        assert_eq!(
            muts.len(),
            1,
            "f(a) must produce exactly 1 arg_removal mutation"
        );
        assert!(
            muts[0].replacement.contains("f(None)"),
            "should produce f(None)"
        );
    }

    // INV-3: f(*args) → 0 arg_removal mutations
    #[test]
    fn test_arg_removal_star_args_skipped() {
        let source = "def foo(args):\n    f(*args)\n";
        let muts = arg_removal_mutations(source);
        assert!(
            muts.is_empty(),
            "f(*args) must produce 0 arg_removal mutations"
        );
    }

    // INV-4: f(**kwargs) → 0 arg_removal mutations
    #[test]
    fn test_arg_removal_double_star_kwargs_skipped() {
        let source = "def foo(kwargs):\n    f(**kwargs)\n";
        let muts = arg_removal_mutations(source);
        assert!(
            muts.is_empty(),
            "f(**kwargs) must produce 0 arg_removal mutations"
        );
    }

    // INV-5: f(None) → 0 arg_removal mutations (already None, only arg so no removal)
    #[test]
    fn test_arg_removal_already_none_single() {
        let source = "def foo():\n    f(None)\n";
        let muts = arg_removal_mutations(source);
        assert!(
            muts.is_empty(),
            "f(None) single arg must produce 0 arg_removal mutations"
        );
    }

    // INV-6: f() → 0 arg_removal mutations
    #[test]
    fn test_arg_removal_empty_call() {
        let source = "def foo():\n    f()\n";
        let muts = arg_removal_mutations(source);
        assert!(muts.is_empty(), "f() must produce 0 arg_removal mutations");
    }

    // INV-7: f(a, b=2) handles keyword args correctly
    #[test]
    fn test_arg_removal_keyword_arg() {
        let source = "def foo(a):\n    f(a, b=2)\n";
        let muts = arg_removal_mutations(source);
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        assert_eq!(
            muts.len(),
            4,
            "f(a, b=2) must produce 4 arg_removal mutations; got: {replacements:?}"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None, b=2)")),
            "missing f(None, b=2)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, b=None)")),
            "missing f(a, b=None)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(b=2)")),
            "missing f(b=2)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a)")),
            "missing f(a)"
        );
    }

    // Three-arg call: f(a, b, c) → 6 mutations (replace each × 3 + remove each × 3)
    #[test]
    fn test_arg_removal_three_args() {
        let source = "def foo(a, b, c):\n    f(a, b, c)\n";
        let muts = arg_removal_mutations(source);
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        assert_eq!(
            muts.len(),
            6,
            "f(a, b, c) must produce 6 arg_removal mutations; got: {replacements:?}"
        );
        // replace mutations
        assert!(
            replacements.iter().any(|r| r.contains("f(None, b, c)")),
            "missing f(None, b, c)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, None, c)")),
            "missing f(a, None, c)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, b, None)")),
            "missing f(a, b, None)"
        );
        // removal mutations
        assert!(
            replacements.iter().any(|r| r.contains("f(b, c)")),
            "missing f(b, c) — remove first"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, c)")),
            "missing f(a, c) — remove middle"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(a, b)")),
            "missing f(a, b) — remove last"
        );
    }

    // None arg in multi-arg call: removal is generated even though replace is skipped
    #[test]
    fn test_arg_removal_none_arg_in_multi_arg() {
        let source = "def foo(b):\n    f(None, b)\n";
        let muts = arg_removal_mutations(source);
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        // arg 0 (None): no replace (already None), but remove → f(b)
        // arg 1 (b): replace → f(None, None), remove → f(None)
        assert_eq!(
            muts.len(),
            3,
            "f(None, b) must produce 3 arg_removal mutations; got: {replacements:?}"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(b)")),
            "missing f(b)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None, None)")),
            "missing f(None, None)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None)")),
            "missing f(None)"
        );
    }

    // INV-8: All generated arg_removal mutations produce syntactically valid Python
    #[test]
    fn test_arg_removal_all_mutations_parseable() {
        let source = "def foo(a, b, c):\n    result = f(a, b, c)\n";
        let fms = collect_file_mutations(source);
        let fm = fms.first().expect("should collect mutations");
        for m in fm.mutations.iter().filter(|m| m.operator == "arg_removal") {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "arg_removal mutation '{}' → '{}' produced unparseable Python:\n{}",
                m.original,
                m.replacement,
                mutated
            );
        }
    }

    // Mixed: star and normal args together — only non-starred args get mutations
    #[test]
    fn test_arg_removal_mixed_star_and_normal() {
        // f(a, *args) — arg 0 is normal, arg 1 is starred
        let source = "def foo(a, args):\n    f(a, *args)\n";
        let muts = arg_removal_mutations(source);
        // arg 0 (a): replace with None (1 mutation); no removal because starred args.len()=2 BUT
        // *args is skipped, so the removal loop sees len=2 > 1 and removes arg 0 → f(*args)
        let replacements: Vec<&str> = muts.iter().map(|m| m.replacement.as_str()).collect();
        // arg 0 produces: replace → f(None, *args), remove → f(*args)
        // arg 1 (*args): skipped entirely
        assert_eq!(
            muts.len(),
            2,
            "f(a, *args) must produce 2 arg_removal mutations; got: {replacements:?}"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(None, *args)")),
            "missing f(None, *args)"
        );
        assert!(
            replacements.iter().any(|r| r.contains("f(*args)")),
            "missing f(*args)"
        );
    }

    // --- annotation skip tests ---

    // INV-1: Type annotations in function parameters produce 0 mutations.
    #[test]
    fn test_annotation_skip_param_types() {
        // int and str appear in the function signature, not the body.
        // The cursor starts past the header so they are never visited.
        let source = "def f(x: int) -> str:\n    return x\n";
        let fms = collect_file_mutations(source);
        // The body only has `return x` — `x` is a Name but not True/False/deepcopy → 0 mutations.
        // Verify no mutations come from `int` or `str` in the signature.
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        let ann_muts: Vec<_> = all_muts
            .iter()
            .filter(|m| m.original == "int" || m.original == "str")
            .collect();
        assert!(
            ann_muts.is_empty(),
            "type annotations must not produce mutations"
        );
    }

    // INV-2: Variable annotation (AnnAssign) produces 0 mutations on the annotation.
    #[test]
    fn test_annotation_skip_ann_assign_type() {
        // `x: int = 5` — the annotation `int` must not be mutated; the value 5 may be.
        let source = "def foo():\n    x: int = 5\n    return x\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        let int_muts: Vec<_> = all_muts.iter().filter(|m| m.original == "int").collect();
        assert!(
            int_muts.is_empty(),
            "annotation 'int' must not produce mutations"
        );
        // The value 5 should produce a number mutation.
        let num_muts: Vec<_> = all_muts
            .iter()
            .filter(|m| m.operator == "number_mutation")
            .collect();
        assert!(
            !num_muts.is_empty(),
            "value '5' in annotation assignment should still be mutated"
        );
    }

    // INV-3: Pure type annotation (no value) produces 0 mutations.
    #[test]
    fn test_annotation_skip_pure_ann_assign() {
        // `x: int` with no value — nothing should be mutated.
        let source = "def foo():\n    x: int\n    return 1 + 1\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        let int_muts: Vec<_> = all_muts.iter().filter(|m| m.original == "int").collect();
        assert!(
            int_muts.is_empty(),
            "pure annotation 'x: int' must produce 0 mutations on int"
        );
    }

    // INV-4: Generic annotation like List[int] produces 0 mutations.
    #[test]
    fn test_annotation_skip_generic() {
        let source = "def foo():\n    x: list = []\n    return x\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        // The annotation `list` is a Name, but should not produce mutations.
        let list_muts: Vec<_> = all_muts.iter().filter(|m| m.original == "list").collect();
        assert!(
            list_muts.is_empty(),
            "annotation 'list' must not produce mutations"
        );
    }

    // --- NEVER_MUTATE_FUNCTION_CALLS tests ---

    // INV-5: len(x) produces 0 mutations (call and argument both skipped).
    #[test]
    fn test_len_call_not_mutated() {
        let source = "def foo(x):\n    return len(x)\n";
        let fms = collect_file_mutations(source);
        // len(x) should produce 0 mutations total (no arg_removal, no method_swap, x not visited).
        assert!(
            fms.is_empty() || fms[0].mutations.is_empty(),
            "len(x) must produce 0 mutations"
        );
    }

    // INV-6: isinstance(x, int) produces 0 mutations.
    #[test]
    fn test_isinstance_call_not_mutated() {
        let source = "def foo(x):\n    return isinstance(x, int)\n";
        let fms = collect_file_mutations(source);
        assert!(
            fms.is_empty() || fms[0].mutations.is_empty(),
            "isinstance(x, int) must produce 0 mutations"
        );
    }

    // INV-7: Regular calls are still mutated (len/isinstance skip is not a general rule).
    #[test]
    fn test_regular_calls_still_mutated() {
        let source = "def foo(x):\n    return list(x)\n";
        let fms = collect_file_mutations(source);
        // list(x) — arg x produces arg_removal mutation (replace with None)
        let all_muts: Vec<_> = fms.iter().flat_map(|fm| fm.mutations.iter()).collect();
        assert!(
            !all_muts.is_empty(),
            "regular calls like list(x) must still produce mutations"
        );
    }

    // INV-8: len() inside a larger expression doesn't block other mutations.
    #[test]
    fn test_len_inside_expression_doesnt_block_other_muts() {
        // a + len(x): the + should still produce a binop_swap mutation.
        let source = "def foo(a, x):\n    return a + len(x)\n";
        let fms = collect_file_mutations(source);
        let binops: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "binop_swap")
            .collect();
        assert!(
            !binops.is_empty(),
            "binop + should still produce a mutation even when len() is present"
        );
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
        assert!(
            has_a_minus,
            "One mutant should be 'a - b + c', got: {mutated0} and {mutated1}"
        );
        assert!(
            has_b_minus,
            "One mutant should be 'a + b - c', got: {mutated0} and {mutated1}"
        );
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
        assert!(
            mutated.contains("a-b"),
            "Should produce a-b, got: {mutated}"
        );
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
        let positions: std::collections::HashSet<usize> = binops.iter().map(|m| m.start).collect();
        assert_eq!(
            positions.len(),
            3,
            "All operators must be at distinct positions"
        );

        // Each mutation should produce syntactically reasonable output
        for m in &binops {
            let mutated = apply_mutation(&fm.source, m);
            // The mutated source should still contain def and return
            assert!(
                mutated.contains("def foo"),
                "Mutated source should still have def"
            );
            assert!(
                mutated.contains("return"),
                "Mutated source should still have return"
            );
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
        assert!(
            mutated.contains("a - a"),
            "Should produce a - a, got: {mutated}"
        );
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

#[cfg(test)]
mod match_case_removal_tests {
    use super::*;

    fn match_case_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "match_case_removal")
            .collect()
    }

    // INV-1: A match with 1 case produces 0 mutations.
    #[test]
    fn test_single_case_no_mutations() {
        let source = "def foo(cmd):\n    match cmd:\n        case _:\n            return 0\n";
        let muts = match_case_mutations(source);
        assert!(muts.is_empty(), "1-case match must produce 0 mutations");
    }

    // INV-2: A match with 2 cases produces 2 mutations.
    #[test]
    fn test_two_cases_two_mutations() {
        let source = "def foo(cmd):\n    match cmd:\n        case \"quit\":\n            return 0\n        case _:\n            return 1\n";
        let muts = match_case_mutations(source);
        assert_eq!(muts.len(), 2, "2-case match must produce 2 mutations");
    }

    // INV-3: A match with 3 cases produces 3 mutations.
    #[test]
    fn test_three_cases_three_mutations() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case \"hello\":\n",
            "            print(\"hi\")\n",
            "        case _:\n",
            "            print(\"unknown\")\n",
        );
        let muts = match_case_mutations(source);
        assert_eq!(muts.len(), 3, "3-case match must produce 3 mutations");
    }

    // INV-4: Generated Python from each mutation is syntactically valid.
    #[test]
    fn test_mutations_produce_valid_python() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case \"hello\":\n",
            "            return 1\n",
            "        case _:\n",
            "            return 2\n",
        );
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
        {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "Removing case produced invalid Python:\n{mutated}"
            );
        }
    }

    // INV-5: Removing case[0] keeps case[1] and case[2].
    #[test]
    fn test_remove_first_case() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case \"hello\":\n",
            "            return 1\n",
            "        case _:\n",
            "            return 2\n",
        );
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        let muts: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
            .collect();
        assert_eq!(muts.len(), 3);

        // The mutation that removes case "quit" should keep the other two cases.
        let mutants: Vec<String> = muts.iter().map(|m| apply_mutation(&fm.source, m)).collect();

        // One mutant drops "quit" branch
        assert!(
            mutants.iter().any(|s| !s.contains("\"quit\"")
                && s.contains("\"hello\"")
                && s.contains("case _")),
            "One mutant should remove 'quit' case while keeping 'hello' and '_'"
        );

        // One mutant drops "hello" branch
        assert!(
            mutants.iter().any(|s| s.contains("\"quit\"")
                && !s.contains("\"hello\"")
                && s.contains("case _")),
            "One mutant should remove 'hello' case while keeping 'quit' and '_'"
        );

        // One mutant drops wildcard branch
        assert!(
            mutants.iter().any(|s| s.contains("\"quit\"")
                && s.contains("\"hello\"")
                && !s.contains("case _")),
            "One mutant should remove '_' case while keeping 'quit' and 'hello'"
        );
    }

    // INV-6: Nested match statements each produce their own mutations independently.
    #[test]
    fn test_nested_match_independent_mutations() {
        let source = concat!(
            "def foo(cmd, sub):\n",
            "    match cmd:\n",
            "        case \"outer_a\":\n",
            "            match sub:\n",
            "                case \"inner_1\":\n",
            "                    return 0\n",
            "                case \"inner_2\":\n",
            "                    return 1\n",
            "        case \"outer_b\":\n",
            "            return 2\n",
        );
        let muts = match_case_mutations(source);
        // Outer match has 2 cases → 2 mutations.
        // Inner match has 2 cases → 2 mutations.
        // Total: 4 match_case_removal mutations.
        assert_eq!(
            muts.len(),
            4,
            "Outer (2 cases) + inner (2 cases) = 4 match_case_removal mutations, got: {muts:?}"
        );
    }

    // INV-7: Span correctness — original text equals func_source[start..end].
    #[test]
    fn test_span_correctness() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case _:\n",
            "            return 1\n",
        );
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
        {
            let slice = &fm.source[m.start..m.end];
            assert_eq!(
                slice, m.original,
                "Span [{}, {}) must equal original",
                m.start, m.end
            );
        }
    }

    // INV-8: Indentation is preserved in remaining cases after removal.
    #[test]
    fn test_indentation_preserved() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"quit\":\n",
            "            return 0\n",
            "        case _:\n",
            "            return 1\n",
        );
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        let muts: Vec<_> = fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
            .collect();
        assert_eq!(muts.len(), 2);

        for m in &muts {
            let mutated = apply_mutation(&fm.source, m);
            // The remaining "case" line must still be indented by 8 spaces.
            assert!(
                mutated.contains("        case "),
                "Case indentation must be preserved: {mutated:?}"
            );
        }
    }

    // --- lambda_mutation splice correctness tests ---

    fn lambda_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "lambda_mutation")
            .collect()
    }

    // INV-1: `lambda x: x if x else None` — body text contains `x` which also appears in
    // params; the mutation must replace only the body, not the parameter.
    #[test]
    fn test_lambda_mutation_body_text_in_params() {
        let source = "def foo():\n    f = lambda x: x\n";
        let muts = lambda_mutations(source);
        assert!(!muts.is_empty(), "should find a lambda mutation");
        let m = &muts[0];
        // The replacement must not corrupt the parameter list.
        assert!(
            m.replacement.contains("lambda x: None"),
            "param `x` must be untouched; replacement was: {}",
            m.replacement
        );
        // The old String::replace() bug would have produced "lambda None: None".
        assert!(
            !m.replacement.contains("lambda None"),
            "param must not be replaced; replacement was: {}",
            m.replacement
        );
    }

    // INV-1 (extended): complex body that includes the param name multiple times
    #[test]
    fn test_lambda_mutation_complex_body_with_param_name() {
        let source = "def foo():\n    f = lambda x: x if x else None\n";
        let fms = collect_file_mutations(source);
        let muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "lambda_mutation")
            .collect();
        assert!(!muts.is_empty(), "should find a lambda mutation");
        let m = &muts[0];
        // Body is `x if x else None` → replacement body is `None` (since body != "None").
        // The param `x` must remain untouched.
        assert!(
            m.replacement.starts_with("lambda x:"),
            "param `x` must be preserved; replacement was: {}",
            m.replacement
        );
        assert!(
            m.replacement.ends_with("None"),
            "body should be replaced with None; replacement was: {}",
            m.replacement
        );
    }

    // INV-2: `lambda: 0` (no params) — body `0` → `None` via lambda mutation
    #[test]
    fn test_lambda_mutation_no_params() {
        let source = "def foo():\n    f = lambda: 0\n";
        let muts = lambda_mutations(source);
        // Lambda body `0` is a number — lambda_mutation replaces it with `None`.
        let lam_mut = muts.iter().find(|m| m.replacement.contains("lambda: None"));
        assert!(
            lam_mut.is_some(),
            "lambda: 0 should produce lambda: None; got: {:?}",
            muts.iter().map(|m| &m.replacement).collect::<Vec<_>>()
        );
    }

    // INV-3: applying any lambda mutation via apply_mutation() must produce parseable Python
    #[test]
    fn test_lambda_mutation_produces_parseable_python() {
        let cases = [
            "def foo():\n    f = lambda x: x\n",
            "def foo():\n    f = lambda x: x if x else None\n",
            "def foo():\n    f = lambda: 0\n",
            "def foo():\n    f = lambda a, b: a + b\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "lambda_mutation") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "lambda mutation produced unparseable Python for input {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-4: `lambda: None` — body `None` → `0` (reverse direction)
    #[test]
    fn test_lambda_mutation_body_none_replaced_with_zero() {
        let source = "def foo():\n    f = lambda: None\n";
        let muts = lambda_mutations(source);
        let lam_mut = muts.iter().find(|m| m.replacement.contains("lambda: 0"));
        assert!(
            lam_mut.is_some(),
            "lambda: None should produce lambda: 0; got: {:?}",
            muts.iter().map(|m| &m.replacement).collect::<Vec<_>>()
        );
    }

    // INV-9: String literal containing `match x:` in a preceding statement does not
    // confuse the match-header search — only the real match generates case removals.
    #[test]
    fn test_preceding_string_with_match_pattern() {
        let source = concat!(
            "def foo(x):\n",
            "    s = \"match x:\"\n", // string literal looks like a match header
            "    match x:\n",
            "        case 1:\n",
            "            return 1\n",
            "        case 2:\n",
            "            return 2\n",
        );
        let muts = match_case_mutations(source);
        // Only the real match (2 cases) should produce mutations.
        assert_eq!(
            muts.len(),
            2,
            "Preceding string with 'match x:' must not generate extra mutations; got: {muts:?}"
        );
    }

    // INV-10: Case body containing `case _:` in a comment does not produce a false match
    // when searching for the next case — the real second case is still correctly found.
    #[test]
    fn test_case_keyword_in_comment_not_matched() {
        let source = concat!(
            "def foo(cmd):\n",
            "    match cmd:\n",
            "        case \"a\":\n",
            "            # TODO: case _: should also handle fallback\n",
            "            return 0\n",
            "        case _:\n",
            "            return 1\n",
        );
        let muts = match_case_mutations(source);
        assert_eq!(
            muts.len(),
            2,
            "Comment containing 'case _:' must not produce a false match; got: {muts:?}"
        );
        // Each mutation must produce valid Python.
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
        {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "Removing case produced invalid Python:\n{mutated}"
            );
        }
    }

    // INV-11: Match with guarded cases (case x if cond:) correctly locates case starts.
    #[test]
    fn test_guarded_cases() {
        let source = concat!(
            "def foo(x):\n",
            "    match x:\n",
            "        case 1 if x > 0:\n",
            "            return 1\n",
            "        case 2 if x > 0:\n",
            "            return 2\n",
            "        case _:\n",
            "            return 3\n",
        );
        let muts = match_case_mutations(source);
        assert_eq!(
            muts.len(),
            3,
            "Guarded cases must each generate a removal mutation; got: {muts:?}"
        );
        // All mutants must parse.
        let fms = collect_file_mutations(source);
        let fm = &fms[0];
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "match_case_removal")
        {
            let mutated = apply_mutation(&fm.source, m);
            assert!(
                parse_module(&mutated, None).is_ok(),
                "Removing guarded case produced invalid Python:\n{mutated}"
            );
        }
    }
}

#[cfg(test)]
mod assignment_mutation_tests {
    use super::*;

    fn assignment_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "assignment_mutation")
            .collect()
    }

    fn assignment_mutants(source: &str) -> Vec<String> {
        let fms = collect_file_mutations(source);
        let fm = fms.into_iter().next().expect("should have mutations");
        fm.mutations
            .iter()
            .filter(|m| m.operator == "assignment_mutation")
            .map(|m| apply_mutation(&fm.source, m))
            .collect()
    }

    // INV-1: `x = y == z` — value contains `==`; first `=` in text is still the assignment `=`.
    // The mutation must produce `x = None`, not a truncated result from matching `=` inside `==`.
    #[test]
    fn test_assignment_value_with_comparison() {
        let source = "def foo(y, z):\n    x = y == z\n";
        let muts = assignment_mutations(source);
        assert_eq!(muts.len(), 1, "should find one assignment mutation");
        // m.replacement is the full replaced span text
        assert_eq!(
            muts[0].replacement, "x = None",
            "replacement should be 'x = None'; got {:?}",
            muts[0].replacement
        );
        let mutants = assignment_mutants(source);
        assert_eq!(mutants.len(), 1);
        assert!(
            mutants[0].contains("x = None"),
            "mutated source should contain 'x = None'; got:\n{}",
            mutants[0]
        );
    }

    // INV-2: `x = d['=']` — value contains a string literal with `=`; must not confuse the splitter.
    #[test]
    fn test_assignment_value_with_eq_in_string() {
        let source = "def foo(d):\n    x = d['=']\n";
        let mutants = assignment_mutants(source);
        assert_eq!(mutants.len(), 1);
        assert!(
            mutants[0].contains("x = None"),
            "mutated source should contain 'x = None'; got:\n{}",
            mutants[0]
        );
    }

    // INV-3: `a = b = c` — chained assignment has two targets.
    // The mutation must replace the value `c` with `None`, preserving both targets: `a = b = None`.
    // The old find('=') approach would produce `a = None`, silently dropping `b` as a target.
    #[test]
    fn test_chained_assignment_preserves_all_targets() {
        let source = "def foo(c):\n    a = b = c\n";
        let mutants = assignment_mutants(source);
        assert_eq!(mutants.len(), 1, "chained assignment should produce exactly one assignment mutation");
        assert!(
            mutants[0].contains("a = b = None"),
            "chained assignment must produce 'a = b = None' (both targets preserved); got:\n{}",
            mutants[0]
        );
    }

    // INV-4: `a, b = 1, 2` — tuple unpacking (single AssignTarget with a Tuple target).
    // The mutation must produce `a, b = None`.
    #[test]
    fn test_tuple_unpacking_assignment() {
        let source = "def foo():\n    a, b = 1, 2\n";
        let mutants = assignment_mutants(source);
        assert_eq!(mutants.len(), 1, "tuple unpacking should produce one assignment mutation");
        assert!(
            mutants[0].contains("a, b = None"),
            "tuple unpacking assignment must produce 'a, b = None'; got:\n{}",
            mutants[0]
        );
    }

    // INV-5: All assignment mutations produce syntactically valid Python.
    #[test]
    fn test_all_assignment_mutations_produce_valid_python() {
        let sources = [
            "def foo(y, z):\n    x = y == z\n",
            "def foo(d):\n    x = d['=']\n",
            "def foo(c):\n    a = b = c\n",
            "def foo():\n    a, b = 1, 2\n",
            "def foo():\n    x = 1\n",
            "def foo():\n    x = None\n",
        ];
        for source in &sources {
            let fms = collect_file_mutations(source);
            if let Some(fm) = fms.first() {
                for m in fm.mutations.iter().filter(|m| m.operator == "assignment_mutation") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "assignment_mutation on {:?} produced unparseable Python:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-6: `x = None` — when the current value is already None, mutate to `""`.
    #[test]
    fn test_assignment_none_to_empty_string() {
        let source = "def foo():\n    x = None\n";
        let muts = assignment_mutations(source);
        assert_eq!(muts.len(), 1);
        // m.replacement is the full replaced span text
        assert_eq!(
            muts[0].replacement, "x = \"\"",
            "when value is None, full replacement must be 'x = \"\"'; got {:?}",
            muts[0].replacement
        );
        let mutants = assignment_mutants(source);
        assert!(
            mutants[0].contains("x = \"\""),
            "must produce 'x = \"\"'; got:\n{}",
            mutants[0]
        );
    }
}
