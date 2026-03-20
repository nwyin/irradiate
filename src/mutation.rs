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
    CompoundStatement, Expression, SmallStatement, Statement, UnaryOp, YieldValue,
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

/// Decorators whose removal is skipped: removing them changes ABC semantics fundamentally
/// (@abstractmethod) or is semantically meaningless for mutation testing (@override).
const SKIP_DECORATOR_REMOVAL: &[&str] = &["abstractmethod", "override"];

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

    // Collect mutations on default argument values in the function signature.
    collect_default_arg_mutations(&func.params, &func_source, &mut mutations);

    collect_suite_mutations(
        &func.body,
        &func_source,
        &mut cursor,
        &mut mutations,
        ignored_lines,
    );

    // Decorator removal mutations: one per non-skipped decorator.
    // Uses a separate forward cursor since decorators precede `def` in func_source.
    let mut dec_cursor: usize = 0;
    for dec in &func.decorators {
        let dec_expr_text = codegen_node(&dec.decorator);
        let dec_expr_trimmed = dec_expr_text.trim();
        let dec_text = codegen_node(dec);

        // Skip decorators that must not be removed.
        if SKIP_DECORATOR_REMOVAL.contains(&dec_expr_trimmed) {
            // Still advance the cursor so subsequent decorators are found correctly.
            if let Some(pos) = func_source[dec_cursor..].find(&dec_text) {
                dec_cursor += pos + dec_text.len();
            }
            continue;
        }

        if let Some(pos) = func_source[dec_cursor..].find(&dec_text) {
            let dec_start = dec_cursor + pos;
            // Replacement is "" — removing the decorator line entirely.
            record_mutation(&dec_text, "", "decorator_removal", dec_start, &mut mutations);
            dec_cursor = dec_start + dec_text.len();
        }
    }

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

// --- Default argument value mutations ---

/// Collect mutations for default argument values in a function's parameter list.
///
/// For each parameter with a default value, generates one mutation that changes
/// the default to a different valid Python value. The mutation offset is computed
/// relative to `func_source` (the full function definition text).
///
/// Strategy: codegen the full params text, then use a monotonic cursor to find each
/// default value in left-to-right order (handles duplicate default values correctly).
fn collect_default_arg_mutations(
    params: &cst::Parameters,
    func_source: &str,
    mutations: &mut Vec<Mutation>,
) {
    let params_text = codegen_node(params);

    // Empty params: nothing to do.
    if params_text.is_empty() {
        return;
    }

    // Find where the params text appears in the function source.
    // The params sit inside "def name(HERE):" so the first occurrence is always correct.
    let params_start = match func_source.find(&params_text) {
        Some(p) => p,
        None => return,
    };

    // Process positional-only, regular, and keyword-only params (those that can have defaults).
    // Skip star_arg (*args) and star_kwarg (**kwargs): they cannot have defaults in Python.
    let all_params = params
        .posonly_params
        .iter()
        .chain(params.params.iter())
        .chain(params.kwonly_params.iter());

    let mut cursor = 0usize; // monotonic cursor within params_text

    for param in all_params {
        let default_expr = match &param.default {
            Some(d) => d,
            None => {
                // No default: advance cursor past the param name to ensure the next
                // name search doesn't backtrack.
                let name = param.name.value;
                if let Some(pos) = params_text[cursor..].find(name) {
                    cursor += pos + name.len();
                }
                continue;
            }
        };

        let default_text = codegen_node(default_expr);

        // Advance cursor past the param name (and optional annotation) before
        // searching for the default. This prevents false matches of the default
        // text in the param name or annotation.
        let name = param.name.value;
        if let Some(name_pos) = params_text[cursor..].find(name) {
            cursor += name_pos + name.len();
        }

        // Find the default text in params_text from the current cursor position.
        let default_pos_in_params = match params_text[cursor..].find(&default_text) {
            Some(p) => cursor + p,
            None => continue,
        };

        cursor = default_pos_in_params + default_text.len();

        let abs_offset = params_start + default_pos_in_params;

        if let Some(replacement) = compute_default_replacement(&default_text) {
            if replacement != default_text {
                record_mutation(&default_text, &replacement, "default_arg", abs_offset, mutations);
            }
        }
    }
}

/// Compute the mutation replacement for a default argument value.
///
/// Rules (applied in order):
/// - `None`           → `""`
/// - `True`           → `False`
/// - `False`          → `True`
/// - integer literal  → `n + 1`
/// - float literal    → `n + 1.0`
/// - string literal   → `"XX{inner}XX"` (or `"XX"` for empty strings)
/// - anything else    → `None`
fn compute_default_replacement(text: &str) -> Option<String> {
    let trimmed = text.trim();

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
        // Only single-quoted (not triple-quoted): a triple-quoted string starts with """/'''.
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

    // Fallback: replace with None (unless it's already None, handled above).
    Some("None".to_string())
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
                let cond_cursor = pos_after_keyword(source, *cursor, "if ");
                add_condition_negation_mutation(&if_stmt.test, source, cond_cursor, mutations);
                collect_expr_mutations(&if_stmt.test, source, cursor, mutations, ignored);
                collect_suite_mutations(&if_stmt.body, source, cursor, mutations, ignored);
                if let Some(ref orelse) = if_stmt.orelse {
                    match orelse.as_ref() {
                        cst::OrElse::Elif(elif) => {
                            let cond_cursor = pos_after_keyword(source, *cursor, "elif ");
                            add_condition_negation_mutation(
                                &elif.test,
                                source,
                                cond_cursor,
                                mutations,
                            );
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
                let cond_cursor = pos_after_keyword(source, *cursor, "while ");
                add_condition_negation_mutation(&w.test, source, cond_cursor, mutations);
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
                let cursor_before = *cursor;
                collect_suite_mutations(&t.body, source, cursor, mutations, ignored);
                for handler in &t.handlers {
                    collect_suite_mutations(&handler.body, source, cursor, mutations, ignored);
                }
                if let Some(ref fin) = t.finalbody {
                    collect_suite_mutations(&fin.body, source, cursor, mutations, ignored);
                }
                // Exception type broadening: except ValueError → except Exception
                add_exception_type_mutations(&t.handlers, source, cursor_before, mutations);
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
                // Pre-find return text before recursing so we have the start position.
                let ret_text = codegen_node(ret);
                let ret_start = source[*cursor..].find(&ret_text).map(|p| *cursor + p);

                collect_expr_mutations(val, source, cursor, mutations, ignored);

                // Return value mutation: `return x → return None` or `return None → return ""`
                if let Some(start) = ret_start {
                    let val_text = codegen_node(val);
                    add_return_value_mutation(&val_text, &ret_text, start, mutations);
                }
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
            let cond_cursor = pos_after_keyword(source, *cursor, "assert ");
            add_condition_negation_mutation(&a.test, source, cond_cursor, mutations);
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
        SmallStatement::Break(b) => {
            let text = codegen_node(b);
            if let Some(pos) = source[*cursor..].find(&text) {
                let start = *cursor + pos;
                record_mutation(&text, "continue", "keyword_swap", start, mutations);
                *cursor = start + text.len();
            }
        }
        SmallStatement::Continue(c) => {
            let text = codegen_node(c);
            if let Some(pos) = source[*cursor..].find(&text) {
                let start = *cursor + pos;
                record_mutation(&text, "break", "keyword_swap", start, mutations);
                *cursor = start + text.len();
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
                add_dict_kwarg_mutations(call, &expr_text, expr_start, mutations);
                collect_expr_mutations(&call.func, source, &mut local, mutations, ignored);
                for arg in &call.args {
                    collect_expr_mutations(&arg.value, source, &mut local, mutations, ignored);
                }
            }
        }
        Expression::IfExp(ifexp) => {
            // Ternary swap: swap body and orelse, keep condition.
            // `a if cond else b` → `b if cond else a`
            let body_text = codegen_node(&*ifexp.body);
            let test_text = codegen_node(&*ifexp.test);
            let orelse_text = codegen_node(&*ifexp.orelse);
            if body_text != orelse_text {
                // Preserve original whitespace around `if` and `else` keywords.
                let ws_before_if = codegen_node(&ifexp.whitespace_before_if);
                let ws_after_if = codegen_node(&ifexp.whitespace_after_if);
                let ws_before_else = codegen_node(&ifexp.whitespace_before_else);
                let ws_after_else = codegen_node(&ifexp.whitespace_after_else);
                let lpar_text: String = ifexp.lpar.iter().map(codegen_node).collect();
                let rpar_text: String = ifexp.rpar.iter().map(codegen_node).collect();
                let replacement = format!(
                    "{lpar_text}{orelse_text}{ws_before_if}if{ws_after_if}{test_text}{ws_before_else}else{ws_after_else}{body_text}{rpar_text}"
                );
                record_mutation(&expr_text, &replacement, "ternary_swap", expr_start, mutations);
            }
            // Source order: body "if" test "else" orelse
            collect_expr_mutations(&ifexp.body, source, &mut local, mutations, ignored);
            // Condition negation: a if cond else b → a if not (cond) else b
            // Search past the "if " keyword that separates body from test.
            let cond_cursor = pos_after_keyword(source, local, "if ");
            add_condition_negation_mutation(&ifexp.test, source, cond_cursor, mutations);
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
        Expression::Yield(y) => {
            // Recurse into the yielded value so operators inside `yield x OP y` are mutated.
            if let Some(ref yv) = y.value {
                match yv.as_ref() {
                    YieldValue::Expression(inner) => {
                        collect_expr_mutations(inner, source, &mut local, mutations, ignored);
                    }
                    YieldValue::From(from) => {
                        collect_expr_mutations(&from.item, source, &mut local, mutations, ignored);
                    }
                }
            }
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

/// Emit a condition_negation mutation for `test_expr` if it is not already a `not` expression.
///
/// `cursor` is the position in `source` to start searching for `test_expr`.  Pass a position
/// that is AFTER the statement keyword (e.g. past `"if "`, `"while "`, `"assert "`) so that
/// single-letter conditions are not found as substrings of the keyword itself.
///
/// Does NOT advance any caller-owned cursor; pass by value.
fn add_condition_negation_mutation(
    test_expr: &Expression,
    source: &str,
    cursor: usize,
    mutations: &mut Vec<Mutation>,
) {
    // Skip if already `not <expr>` — negating a negation is equivalent to removing `not`,
    // which `unary_removal` already covers.
    if let Expression::UnaryOperation(unop) = test_expr {
        if matches!(unop.operator, UnaryOp::Not { .. }) {
            return;
        }
    }
    let test_text = codegen_node(test_expr);
    if let Some(pos) = source[cursor..].find(&test_text) {
        let start = cursor + pos;
        let replacement = format!("not ({})", test_text);
        record_mutation(&test_text, &replacement, "condition_negation", start, mutations);
    }
}

/// Return the byte offset in `source` immediately after the first occurrence of `keyword`
/// starting from `cursor`.  Falls back to `cursor` if not found (e.g. malformed source).
fn pos_after_keyword(source: &str, cursor: usize, keyword: &str) -> usize {
    source[cursor..]
        .find(keyword)
        .map(|p| cursor + p + keyword.len())
        .unwrap_or(cursor)
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
        UnaryOp::Plus { .. } => {
            // +x → -x
            let operand_text = codegen_node(&*unop.expression);
            let replacement = format!("-{}", operand_text);
            record_mutation(full_text, &replacement, "unary_swap", start, mutations);
        }
        UnaryOp::Minus { .. } => {
            // -x → +x
            let operand_text = codegen_node(&*unop.expression);
            let replacement = format!("+{}", operand_text);
            record_mutation(full_text, &replacement, "unary_swap", start, mutations);
        }
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

    // String emptying: "foo" → ""
    if !inner.is_empty() {
        let empty = format!("{prefix}{quote_char}{quote_char}");
        record_mutation(text, &empty, "string_emptying", start, mutations);
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

/// Return value mutation: `return x → return None` or `return None → return ""`.
///
/// The mutation span covers only the value portion of the return statement, not the
/// `return` keyword itself.  `val_text` is `codegen_node(val)`, `ret_text` is
/// `codegen_node(ret)` (the full "return <value>" text), and `ret_start` is the byte
/// offset of the return statement in the function source.
fn add_return_value_mutation(
    val_text: &str,
    ret_text: &str,
    ret_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    let replacement = if val_text.trim() == "None" { "\"\"" } else { "None" };

    // Locate the value within ret_text by skipping "return" + whitespace.
    let return_kw_len = "return".len();
    if ret_text.len() <= return_kw_len {
        return; // malformed
    }
    let after_return = &ret_text[return_kw_len..];
    let ws_len = after_return.len() - after_return.trim_start().len();
    let val_offset = return_kw_len + ws_len;

    // Safety: verify the value fits within the return text.
    if val_offset + val_text.len() > ret_text.len() {
        return;
    }

    let val_start = ret_start + val_offset;
    record_mutation(val_text, replacement, "return_value", val_start, mutations);
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

/// Conditional method swaps: split ↔ rsplit, but only when a maxsplit argument is present.
/// Without maxsplit, split and rsplit produce identical results, so swapping them is not
/// a meaningful mutation.
static CONDITIONAL_METHOD_SWAPS: &[(&str, &str)] = &[("split", "rsplit"), ("rsplit", "split")];

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

        // Conditional split ↔ rsplit: only swap when maxsplit is involved.
        // split(",") and rsplit(",") always produce the same result, so swapping them is
        // only a meaningful mutation when a maxsplit argument is present.
        for &(from, to) in CONDITIONAL_METHOD_SWAPS {
            if method_trimmed == from {
                let positional_count = call.args.iter().filter(|a| a.keyword.is_none()).count();
                let has_maxsplit_kwarg = call
                    .args
                    .iter()
                    .any(|a| a.keyword.as_ref().is_some_and(|kw| kw.value == "maxsplit"));
                if positional_count == 2 || has_maxsplit_kwarg {
                    let func_text = codegen_node(&*call.func);
                    let dot_pos =
                        func_text.rfind('.').expect("Attribute node always contains a dot");
                    let after_dot = &func_text[dot_pos + 1..];
                    let leading_ws = after_dot.len() - after_dot.trim_start().len();
                    let method_start = dot_pos + 1 + leading_ws;
                    record_mutation(from, to, "method_swap", expr_start + method_start, mutations);
                }
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

/// Generate dict keyword-name mutations for `dict(key=value)` calls.
///
/// For each keyword argument in a `dict(...)` call, emits a mutation that appends "XX"
/// to the keyword name: `dict(foo=1)` → `dict(fooXX=1)`.
///
/// Only fires for calls to the bare name `dict` — not for arbitrary callables with keyword args.
/// Positional args and starred args (`**extra`) are skipped.
fn add_dict_kwarg_mutations(
    call: &cst::Call,
    call_text: &str,
    expr_start: usize,
    mutations: &mut Vec<Mutation>,
) {
    // Only mutate `dict(...)` calls.
    if !matches!(&*call.func, Expression::Name(n) if n.value == "dict") {
        return;
    }

    for arg in call.args.iter() {
        let kw = match &arg.keyword {
            Some(k) => k,
            None => continue, // positional or **splat — skip
        };
        // Skip **kwargs (double-starred args have keyword but also star="**").
        if !arg.star.is_empty() {
            continue;
        }

        let kw_name = kw.value;
        let mutated_name = format!("{kw_name}XX");

        // The mutation spans the keyword name only. Find it inside call_text.
        // The keyword appears as `<kw_name>=` after the opening `(`. Python requires
        // unique keyword names in a call, so the first occurrence is always correct.
        let open_paren = call_text.find('(').unwrap_or(0);
        let needle = format!("{kw_name}=");
        if let Some(rel) = call_text[open_paren..].find(needle.as_str()) {
            let kw_start = open_paren + rel;
            record_mutation(kw_name, &mutated_name, "dict_kwarg", expr_start + kw_start, mutations);
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

// --- Exception type mutations ---

/// Generate exception type broadening mutations for all typed handlers in a try statement.
///
/// For each `except SomeException:` handler, emits a mutation that replaces the exception
/// type with `Exception` (the broadest base class). Bare `except:` handlers and handlers
/// already using `Exception` are skipped.
///
/// Byte offset computation: for each handler, we construct the exact `except<ws><type>` text
/// using the CST-stored `whitespace_after_except` field and search forward from `cursor_before`
/// (which is at or before the try block in the function source). A sub-cursor advances through
/// each handler header so that repeated handler types (e.g. two `except ValueError:`) are
/// located at their correct respective positions.
fn add_exception_type_mutations<'a>(
    handlers: &[cst::ExceptHandler<'a>],
    source: &str,
    cursor_before: usize,
    mutations: &mut Vec<Mutation>,
) {
    let mut search_from = cursor_before;

    for handler in handlers {
        let type_expr = match &handler.r#type {
            Some(t) => t,
            None => {
                // Bare `except:` — no type to broaden. Advance past this handler's keyword.
                if let Some(pos) = source[search_from..].find("except:") {
                    search_from += pos + "except:".len();
                }
                continue;
            }
        };

        let type_text = codegen_node(type_expr);
        let ws = handler.whitespace_after_except.0;
        let pattern = format!("except{ws}{type_text}");

        let pos = match source[search_from..].find(&pattern) {
            Some(p) => search_from + p,
            None => continue,
        };
        // Advance sub-cursor past this handler header so the next handler search starts here.
        search_from = pos + pattern.len();

        if type_text.trim() == "Exception" {
            // Already broadest — skip mutation.
            continue;
        }

        let type_start = pos + "except".len() + ws.len();
        record_mutation(&type_text, "Exception", "exception_type", type_start, mutations);
    }
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
    fn test_decorated_functions_are_collected() {
        // Decorated functions are now fully processed: body mutations + decorator_removal.
        let source = "@decorator\ndef foo():\n    return 1 + 2\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1, "Decorated function should be collected");
        let dec_removals: Vec<_> = fms[0]
            .mutations
            .iter()
            .filter(|m| m.operator == "decorator_removal")
            .collect();
        assert_eq!(dec_removals.len(), 1, "Should have one decorator_removal mutation");
        assert_eq!(dec_removals[0].original, "@decorator\n");
        assert_eq!(dec_removals[0].replacement, "");
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
        // NOTE: return_value mutations on the return statement are acceptable — only the
        // call-level arg_removal/method_swap mutations should be suppressed.
        let non_rv_mutations: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator != "return_value")
            .collect();
        assert!(
            non_rv_mutations.is_empty(),
            "len(x) must produce 0 non-return_value mutations, got: {:?}",
            non_rv_mutations
        );
    }

    // INV-6: isinstance(x, int) produces 0 mutations.
    #[test]
    fn test_isinstance_call_not_mutated() {
        let source = "def foo(x):\n    return isinstance(x, int)\n";
        let fms = collect_file_mutations(source);
        // isinstance(x, int) should produce 0 call-level mutations — only return_value
        // mutations on the enclosing return statement are acceptable.
        let non_rv_mutations: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator != "return_value")
            .collect();
        assert!(
            non_rv_mutations.is_empty(),
            "isinstance(x, int) must produce 0 non-return_value mutations, got: {:?}",
            non_rv_mutations
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
                for m in fm
                    .mutations
                    .iter()
                    .filter(|m| m.operator == "lambda_mutation")
                {
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

// --- Unary operation mutation tests ---
#[cfg(test)]
mod unary_mutation_tests {
    use super::*;

    fn unary_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "unary_removal")
            .collect()
    }

    // INV-1: `not x` → `x` removes the unary `not` operator.
    #[test]
    fn test_not_removal() {
        let source = "def foo(x):\n    return not x\n";
        let muts = unary_mutations(source);
        assert!(!muts.is_empty(), "should find unary_removal mutation for `not x`");
        let m = &muts[0];
        assert_eq!(m.original, "not x", "original should be the full `not x` expression");
        assert_eq!(m.replacement, "x", "replacement should be just `x`");
    }

    // INV-2: `~x` → `x` removes the bitwise invert operator.
    #[test]
    fn test_bit_invert_removal() {
        let source = "def foo(x):\n    return ~x\n";
        let muts = unary_mutations(source);
        assert!(!muts.is_empty(), "should find unary_removal mutation for `~x`");
        let m = &muts[0];
        assert_eq!(m.replacement, "x", "replacement should be just `x`");
    }

    // INV-3: Unary `-` is NOT removed (only `not` and `~` are removed).
    #[test]
    fn test_minus_not_removed() {
        let source = "def foo(x):\n    return -x\n";
        let muts = unary_mutations(source);
        assert!(muts.is_empty(), "unary minus must not produce unary_removal mutation");
    }

    // INV-4: Correct byte span — source[start..end] == original.
    #[test]
    fn test_unary_span_correctness() {
        let source = "def foo(x):\n    return not x\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        for m in fm.mutations.iter().filter(|m| m.operator == "unary_removal") {
            let span_text = &fm.source[m.start..m.end];
            assert_eq!(
                span_text, m.original,
                "INV-4: span [{}, {}) = {:?} but original = {:?}",
                m.start, m.end, span_text, m.original
            );
        }
    }

    // INV-5: All unary mutations produce parseable Python.
    #[test]
    fn test_unary_mutation_produces_parseable_python() {
        let cases = [
            "def foo(x):\n    return not x\n",
            "def foo(x):\n    return ~x\n",
            "def foo(a, x):\n    return not x and a > 0\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "unary_removal") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "unary_removal mutation produced unparseable Python for {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-6: `not x` mutation is found even when inside a compound expression.
    #[test]
    fn test_not_inside_and_expression() {
        let source = "def foo(a, b):\n    return not a and b > 0\n";
        let muts = unary_mutations(source);
        assert!(!muts.is_empty(), "unary_removal should be found inside compound expression");
    }
}

// --- Unary swap mutation tests ---
#[cfg(test)]
mod unary_swap_tests {
    use super::*;

    fn unary_swap_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "unary_swap")
            .collect()
    }

    // INV-1: `-x` → `+x`
    #[test]
    fn test_minus_swapped_to_plus() {
        let source = "def foo(x):\n    return -x\n";
        let muts = unary_swap_mutations(source);
        assert!(!muts.is_empty(), "should find unary_swap for `-x`");
        let m = &muts[0];
        assert_eq!(m.original, "-x");
        assert_eq!(m.replacement, "+x");
    }

    // INV-2: `+x` → `-x`
    #[test]
    fn test_plus_swapped_to_minus() {
        let source = "def foo(x):\n    return +x\n";
        let muts = unary_swap_mutations(source);
        assert!(!muts.is_empty(), "should find unary_swap for `+x`");
        let m = &muts[0];
        assert_eq!(m.original, "+x");
        assert_eq!(m.replacement, "-x");
    }

    // INV-3: `-5` → `+5` (literal numbers)
    #[test]
    fn test_minus_literal_swapped_to_plus() {
        let source = "def foo():\n    return -5\n";
        let muts = unary_swap_mutations(source);
        assert!(!muts.is_empty(), "should find unary_swap for `-5`");
        assert_eq!(muts[0].replacement, "+5");
    }

    // INV-4: `-x` produces both unary_swap (+x) and existing unary_removal (x)
    #[test]
    fn test_minus_produces_both_swap_and_removal_not_produced() {
        // unary_removal only applies to `not` and `~`, not `-`
        let source = "def foo(x):\n    return -x\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.into_iter().flat_map(|fm| fm.mutations.into_iter()).collect();
        let swaps: Vec<_> = all_muts.iter().filter(|m| m.operator == "unary_swap").collect();
        let removals: Vec<_> = all_muts.iter().filter(|m| m.operator == "unary_removal").collect();
        assert!(!swaps.is_empty(), "should have unary_swap for `-x`");
        assert!(removals.is_empty(), "unary_removal must NOT fire for `-x`");
    }

    // INV-5: `not` and `~` do NOT get unary_swap
    #[test]
    fn test_not_and_bitnot_do_not_get_swap() {
        let source_not = "def foo(x):\n    return not x\n";
        let source_inv = "def foo(x):\n    return ~x\n";
        assert!(unary_swap_mutations(source_not).is_empty(), "`not x` must not get unary_swap");
        assert!(unary_swap_mutations(source_inv).is_empty(), "`~x` must not get unary_swap");
    }

    // INV-6: All unary_swap mutations produce parseable Python.
    #[test]
    fn test_unary_swap_produces_parseable_python() {
        let cases = [
            "def foo(x):\n    return -x\n",
            "def foo(x):\n    return +x\n",
            "def foo():\n    return -5\n",
            "def foo(x, y):\n    return -(x + y)\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "unary_swap") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "unary_swap produced unparseable Python for {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }
}

// --- String emptying mutation tests ---
#[cfg(test)]
mod string_emptying_tests {
    use super::*;

    fn string_emptying_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "string_emptying")
            .collect()
    }

    // INV-1: Non-empty string gets both string_mutation (XX) and string_emptying ("") mutations.
    #[test]
    fn test_nonempty_string_gets_both_mutations() {
        let source = "def greet():\n    return \"hello\"\n";
        let fms = collect_file_mutations(source);
        let all_muts: Vec<_> = fms.into_iter().flat_map(|fm| fm.mutations.into_iter()).collect();
        let xx_muts: Vec<_> = all_muts.iter().filter(|m| m.operator == "string_mutation").collect();
        let empty_muts: Vec<_> = all_muts.iter().filter(|m| m.operator == "string_emptying").collect();
        assert!(!xx_muts.is_empty(), "should find string_mutation (XX) for non-empty string");
        assert!(!empty_muts.is_empty(), "should find string_emptying for non-empty string");
        assert_eq!(empty_muts[0].replacement, "\"\"", "emptying replacement should be empty string");
    }

    // INV-2: Already-empty string does NOT get string_emptying (skip if already empty).
    #[test]
    fn test_already_empty_string_not_emptied() {
        let source = "def foo():\n    return \"\"\n";
        let muts = string_emptying_mutations(source);
        assert!(muts.is_empty(), "empty string should not get string_emptying mutation");
    }

    // INV-3: Quote character is preserved in emptied string.
    #[test]
    fn test_empty_uses_same_quote_char() {
        let source = "def foo():\n    return 'hello'\n";
        let muts = string_emptying_mutations(source);
        assert!(!muts.is_empty(), "single-quoted string should get string_emptying");
        assert_eq!(muts[0].replacement, "''", "should use single quotes for emptied string");
    }

    // INV-4: Triple-quoted strings (docstrings) do NOT get string_emptying.
    #[test]
    fn test_triple_quoted_strings_not_emptied() {
        let source = "def foo():\n    \"\"\"This is a docstring.\"\"\"\n    return 1\n";
        let muts = string_emptying_mutations(source);
        assert!(muts.is_empty(), "docstrings must not get string_emptying");
    }

    // INV-5: All string_emptying mutations produce parseable Python.
    #[test]
    fn test_string_emptying_produces_parseable_python() {
        let cases = [
            "def greet():\n    return \"hello\"\n",
            "def foo():\n    return 'world'\n",
            "def bar(x):\n    return x.replace('a', 'b')\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "string_emptying") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "string_emptying produced unparseable Python for {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }
}

// --- Float mutation tests ---
#[cfg(test)]
mod float_mutation_tests {
    use super::*;

    fn float_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "number_mutation")
            .collect()
    }

    // INV-1: `1.5` → `2.5` (float + 1.0).
    #[test]
    fn test_float_incremented_by_one() {
        let source = "def foo():\n    return 1.5\n";
        let muts = float_mutations(source);
        assert!(!muts.is_empty(), "should find number_mutation for float 1.5");
        let m = &muts[0];
        assert_eq!(m.replacement, "2.5", "1.5 should become 2.5");
    }

    // INV-2: `0.0` → `1.0`.
    #[test]
    fn test_float_zero_incremented() {
        let source = "def foo():\n    return 0.0\n";
        let muts = float_mutations(source);
        assert!(!muts.is_empty(), "should find number_mutation for float 0.0");
        let m = &muts[0];
        assert_eq!(m.replacement, "1", "0.0 should become 1 (1.0 after formatting)");
    }

    // INV-3: Float mutation produces parseable Python.
    #[test]
    fn test_float_mutation_parseable_python() {
        let cases = ["def foo():\n    return 1.5\n", "def foo():\n    return 0.0\n"];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "number_mutation") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "float mutation produced unparseable Python for {:?}:\n{}",
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-4: Correct byte span for float.
    #[test]
    fn test_float_span_correctness() {
        let source = "def foo():\n    return 1.5\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        for m in fm.mutations.iter().filter(|m| m.operator == "number_mutation") {
            let span_text = &fm.source[m.start..m.end];
            assert_eq!(
                span_text, m.original,
                "span [{}, {}) = {:?} but original = {:?}",
                m.start, m.end, span_text, m.original
            );
        }
    }
}

// --- AugAssign mutation tests ---
#[cfg(test)]
mod augassign_mutation_tests {
    use super::*;

    fn augop_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "augop_swap")
            .collect()
    }

    fn augassign_to_assign_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "augassign_to_assign")
            .collect()
    }

    // INV-1: `a += b` → `a -= b` (augop_swap).
    #[test]
    fn test_add_assign_swapped_to_sub_assign() {
        let source = "def foo(a, b):\n    a += b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty(), "should find augop_swap for +=");
        assert!(muts.iter().any(|m| m.replacement.contains("-=")), "should swap += to -=");
    }

    // INV-2: `a -= b` → `a += b`.
    #[test]
    fn test_sub_assign_swapped_to_add_assign() {
        let source = "def foo(a, b):\n    a -= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("+=")), "should swap -= to +=");
    }

    // INV-3: `a *= b` → `a /= b`.
    #[test]
    fn test_mul_assign_swapped_to_div_assign() {
        let source = "def foo(a, b):\n    a *= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("/=")), "should swap *= to /=");
    }

    // INV-4: `a //= b` → `a /= b`.
    #[test]
    fn test_floordiv_assign_swapped() {
        let source = "def foo(a, b):\n    a //= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        // //= → /= (trimmed comparison)
        assert!(muts.iter().any(|m| m.replacement.trim() == "/="), "should swap //= to /=");
    }

    // INV-5: `a **= b` → `a *= b`.
    #[test]
    fn test_pow_assign_swapped_to_mul_assign() {
        let source = "def foo(a, b):\n    a **= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("*=")), "should swap **= to *=");
    }

    // INV-6: `a <<= b` → `a >>= b`.
    #[test]
    fn test_lshift_assign_swapped_to_rshift_assign() {
        let source = "def foo(a, b):\n    a <<= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains(">>=")), "should swap <<= to >>=");
    }

    // INV-7: `a >>= b` → `a <<= b`.
    #[test]
    fn test_rshift_assign_swapped_to_lshift_assign() {
        let source = "def foo(a, b):\n    a >>= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("<<=")), "should swap >>= to <<=");
    }

    // INV-8: `a &= b` → `a |= b`.
    #[test]
    fn test_and_assign_swapped_to_or_assign() {
        let source = "def foo(a, b):\n    a &= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("|=")), "should swap &= to |=");
    }

    // INV-9: `a |= b` → `a &= b`.
    #[test]
    fn test_or_assign_swapped_to_and_assign() {
        let source = "def foo(a, b):\n    a |= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("&=")), "should swap |= to &=");
    }

    // INV-10: `a ^= b` → `a &= b`.
    #[test]
    fn test_xor_assign_swapped_to_and_assign() {
        let source = "def foo(a, b):\n    a ^= b\n    return a\n";
        let muts = augop_mutations(source);
        assert!(!muts.is_empty());
        assert!(muts.iter().any(|m| m.replacement.contains("&=")), "should swap ^= to &=");
    }

    // INV-11: `a += b` → `a = b` (augassign_to_assign).
    #[test]
    fn test_augassign_to_assign_conversion() {
        let source = "def foo(a, b):\n    a += b\n    return a\n";
        let muts = augassign_to_assign_mutations(source);
        assert!(!muts.is_empty(), "should find augassign_to_assign mutation");
        // The replacement should be `a = b` (the plain assignment form).
        assert!(
            muts.iter().any(|m| m.replacement.contains("a =") && !m.replacement.contains("+=")),
            "augassign_to_assign should produce plain `a = b`; got: {:?}",
            muts.iter().map(|m| &m.replacement).collect::<Vec<_>>()
        );
    }

    // INV-12: All augop mutations produce parseable Python.
    #[test]
    fn test_augop_mutations_parseable() {
        let cases = [
            "def foo(a, b):\n    a += b\n    return a\n",
            "def foo(a, b):\n    a -= b\n    return a\n",
            "def foo(a, b):\n    a *= b\n    return a\n",
            "def foo(a, b):\n    a //= b\n    return a\n",
            "def foo(a, b):\n    a **= b\n    return a\n",
            "def foo(a, b):\n    a <<= b\n    return a\n",
            "def foo(a, b):\n    a >>= b\n    return a\n",
            "def foo(a, b):\n    a &= b\n    return a\n",
            "def foo(a, b):\n    a |= b\n    return a\n",
            "def foo(a, b):\n    a ^= b\n    return a\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm
                    .mutations
                    .iter()
                    .filter(|m| m.operator == "augop_swap" || m.operator == "augassign_to_assign")
                {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "augop mutation {:?} produced unparseable Python for {:?}:\n{}",
                        m.operator,
                        source,
                        mutated
                    );
                }
            }
        }
    }

    // INV-13: Correct byte span for augop — source[start..end] == original.
    #[test]
    fn test_augop_span_correctness() {
        let source = "def foo(a, b):\n    a += b\n    return a\n";
        let fms = collect_file_mutations(source);
        let fm = fms.first().expect("should collect mutations");
        for m in fm
            .mutations
            .iter()
            .filter(|m| m.operator == "augop_swap" || m.operator == "augassign_to_assign")
        {
            let span_text = &fm.source[m.start..m.end];
            assert_eq!(
                span_text, m.original,
                "span [{}, {}) = {:?} but original = {:?}",
                m.start, m.end, span_text, m.original
            );
        }
    }
}

// --- IfExp (ternary) mutation tests ---
#[cfg(test)]
mod ifexp_mutation_tests {
    use super::*;

    // INV-1: `x + 1 if True else y - 1` — mutations found for both `+` and `-` inside ternary.
    #[test]
    fn test_ifexp_recurses_into_body_and_orelse() {
        let source = "def foo(x, y):\n    return x + 1 if True else y - 1\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should collect mutations from ifexp");
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        // Should find both the `+` in `x + 1` and the `-` in `y - 1`
        assert!(binops.len() >= 2, "should find binop mutations inside ternary body and orelse");
        let has_add = binops.iter().any(|m| m.original.trim() == "+");
        let has_sub = binops.iter().any(|m| m.original.trim() == "-");
        assert!(has_add, "should find + → - mutation in ternary body");
        assert!(has_sub, "should find - → + mutation in ternary orelse");
    }

    // INV-2: Mutations inside ternary produce parseable Python.
    #[test]
    fn test_ifexp_mutations_parseable() {
        let source = "def foo(x, y):\n    return x + 1 if True else y - 1\n";
        let fms = collect_file_mutations(source);
        for fm in &fms {
            for m in &fm.mutations {
                let mutated = apply_mutation(&fm.source, m);
                assert!(
                    parse_module(&mutated, None).is_ok(),
                    "ifexp mutation {:?} produced unparseable Python:\n{}",
                    m.operator,
                    mutated
                );
            }
        }
    }
}

// --- Container literal recursion tests ---
#[cfg(test)]
mod container_mutation_tests {
    use super::*;

    // INV-1: Tuple — mutations found inside tuple elements.
    #[test]
    fn test_tuple_elements_mutated() {
        let source = "def foo(a, b, c, d):\n    return (a + b, c * d)\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        // should find `+` and `*`
        assert!(binops.len() >= 2, "should find binop mutations inside tuple elements");
        let has_add = binops.iter().any(|m| m.original.trim() == "+");
        let has_mul = binops.iter().any(|m| m.original.trim() == "*");
        assert!(has_add, "should mutate `+` inside tuple");
        assert!(has_mul, "should mutate `*` inside tuple");
    }

    // INV-2: Empty tuple must not crash.
    #[test]
    fn test_empty_tuple_no_crash() {
        let source = "def foo():\n    return ()\n";
        let fms = collect_file_mutations(source);
        // No binop mutations; function may be excluded (no mutable ops). Just must not crash.
        let _ = fms;
    }

    // INV-3: List — mutations found inside list elements.
    #[test]
    fn test_list_elements_mutated() {
        let source = "def foo(a, b, c, d):\n    return [a + b, c - d]\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        assert!(binops.len() >= 2, "should find binop mutations inside list elements");
        let has_add = binops.iter().any(|m| m.original.trim() == "+");
        let has_sub = binops.iter().any(|m| m.original.trim() == "-");
        assert!(has_add, "should mutate `+` inside list");
        assert!(has_sub, "should mutate `-` inside list");
    }

    // INV-4: Empty list must not crash.
    #[test]
    fn test_empty_list_no_crash() {
        let source = "def foo():\n    return []\n";
        let fms = collect_file_mutations(source);
        let _ = fms;
    }

    // INV-5: Dict — mutations found in dict values.
    #[test]
    fn test_dict_value_mutated() {
        let source = "def foo(a, b):\n    return {'key': a + b}\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        assert!(!binops.is_empty(), "should find binop mutation inside dict value");
    }

    // INV-6: Empty dict must not crash.
    #[test]
    fn test_empty_dict_no_crash() {
        let source = "def foo():\n    return {}\n";
        let fms = collect_file_mutations(source);
        let _ = fms;
    }

    // INV-7: Subscript — mutations found in sub.value (the subscripted object).
    // The subscript arm recurses into sub.value, so mutations on the object are found.
    // Note: the slice expression is NOT recursed into by the current implementation.
    #[test]
    fn test_subscript_value_mutated() {
        // d.lower()[0] — subscript arm recurses into sub.value = d.lower() (a Call),
        // which produces a method_swap mutation for .lower() → .upper().
        let source = "def foo(d):\n    return d.lower()[0]\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let method_muts: Vec<_> =
            fm.mutations.iter().filter(|m| m.operator == "method_swap").collect();
        assert!(
            !method_muts.is_empty(),
            "subscript arm should recurse into sub.value and find method_swap mutation"
        );
    }

    // INV-8: All container literal mutations produce parseable Python.
    #[test]
    fn test_container_mutations_parseable() {
        let cases = [
            "def foo(a, b, c, d):\n    return (a + b, c * d)\n",
            "def foo(a, b, c, d):\n    return [a + b, c - d]\n",
            "def foo(a, b):\n    return {'key': a + b}\n",
            "def foo(d):\n    return d.lower()[0]\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in &fm.mutations {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "container mutation {:?} produced unparseable Python for {:?}:\n{}",
                        m.operator,
                        source,
                        mutated
                    );
                }
            }
        }
    }
}

// --- Assert statement mutation tests ---
#[cfg(test)]
mod assert_mutation_tests {
    use super::*;

    // INV-1: `assert x + 1` — `+` inside assert test should be mutated.
    #[test]
    fn test_assert_test_expression_mutated() {
        let source = "def foo(x):\n    assert x + 1\n    return x\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let binops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        assert!(!binops.is_empty(), "binop inside assert test should be mutated");
    }

    // INV-2: Assert mutation produces parseable Python.
    #[test]
    fn test_assert_mutation_parseable() {
        let source = "def foo(x):\n    assert x + 1\n    return x\n";
        let fms = collect_file_mutations(source);
        for fm in &fms {
            for m in &fm.mutations {
                let mutated = apply_mutation(&fm.source, m);
                assert!(
                    parse_module(&mutated, None).is_ok(),
                    "assert mutation {:?} produced unparseable Python:\n{}",
                    m.operator,
                    mutated
                );
            }
        }
    }

    // INV-3: `assert a > b` — comparison inside assert is also mutated.
    #[test]
    fn test_assert_comparison_mutated() {
        let source = "def foo(a, b):\n    assert a > b\n    return a\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let compops: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "compop_swap").collect();
        assert!(!compops.is_empty(), "comparison inside assert test should be mutated");
    }
}

// --- Yield detection tests ---
#[cfg(test)]
mod yield_detection_tests {
    use super::*;

    // Helper: parse a function def and call suite_contains_yield.
    fn check_yield_in_source(source: &str) -> bool {
        let module = parse_module(source, None).expect("valid Python");
        for stmt in &module.body {
            if let Statement::Compound(CompoundStatement::FunctionDef(func)) = stmt {
                return suite_contains_yield(&func.body);
            }
        }
        panic!("no function def found in source");
    }

    // INV-1: `yield` inside an `if` block → detected.
    #[test]
    fn test_yield_inside_if_detected() {
        // Note: we need a mutable expr to ensure collect_file_mutations works if called,
        // but here we directly test suite_contains_yield.
        let source = "def gen():\n    if True:\n        yield 1\n";
        assert!(check_yield_in_source(source), "yield inside if must be detected");
    }

    // INV-2: `yield` inside a `while` loop → detected.
    #[test]
    fn test_yield_inside_while_detected() {
        let source = "def gen():\n    while True:\n        yield 1\n";
        assert!(check_yield_in_source(source), "yield inside while must be detected");
    }

    // INV-3: `yield` inside a `for` loop → detected.
    #[test]
    fn test_yield_inside_for_detected() {
        let source = "def gen(items):\n    for x in items:\n        yield x\n";
        assert!(check_yield_in_source(source), "yield inside for must be detected");
    }

    // INV-4: `yield` inside a `with` block → detected.
    #[test]
    fn test_yield_inside_with_detected() {
        let source = "def gen(f):\n    with open(f) as h:\n        yield h.read()\n";
        assert!(check_yield_in_source(source), "yield inside with must be detected");
    }

    // INV-5: `yield` inside `try/except` → detected.
    #[test]
    fn test_yield_inside_try_detected() {
        let source = "def gen():\n    try:\n        yield 1\n    except Exception:\n        pass\n";
        assert!(check_yield_in_source(source), "yield inside try must be detected");
    }

    // INV-6: `yield` inside a nested `def` → NOT detected (must not recurse past FunctionDef).
    #[test]
    fn test_yield_inside_nested_def_not_detected() {
        let source = "def outer():\n    def inner():\n        yield 1\n    return 0\n";
        assert!(
            !check_yield_in_source(source),
            "yield inside nested def must NOT make outer a generator"
        );
    }

    // INV-7: No yield anywhere → not detected.
    #[test]
    fn test_no_yield_not_detected() {
        let source = "def foo():\n    return 1 + 2\n";
        assert!(!check_yield_in_source(source), "function without yield must not be detected");
    }

    // INV-8: `yield from` → detected.
    #[test]
    fn test_yield_from_detected() {
        let source = "def gen(items):\n    yield from items\n";
        assert!(check_yield_in_source(source), "yield from must be detected");
    }

    // INV-9: Top-level `yield` (simple return body style) → detected.
    #[test]
    fn test_top_level_yield_detected() {
        let source = "def gen():\n    yield 1\n";
        assert!(check_yield_in_source(source), "top-level yield must be detected");
    }

    // INV-10: `yield` inside `except` handler (not in body) → detected.
    #[test]
    fn test_yield_inside_except_handler_detected() {
        let source = "def gen():\n    try:\n        pass\n    except Exception:\n        yield 0\n";
        assert!(check_yield_in_source(source), "yield inside except handler must be detected");
    }

    // INV-11: `yield` only inside nested def — outer is_generator flag is correctly False.
    // Exercises the is_generator field of FunctionMutations by collecting mutations.
    #[test]
    fn test_outer_is_generator_false_when_yield_only_in_nested_def() {
        // outer needs a mutation so it gets collected; use a comparison.
        let source = "def outer(n):\n    if n > 0:\n        def inner():\n            yield n\n    return n\n";
        let fms = collect_file_mutations(source);
        let outer = fms.iter().find(|fm| fm.name == "outer").expect("outer should be collected");
        assert!(
            !outer.is_generator,
            "outer must not be is_generator just because nested def has yield"
        );
    }

    // --- default_arg tests ---

    #[test]
    fn test_default_int_incremented() {
        let source = "def f(x=0):\n    return x\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let m = fms[0].mutations.iter().find(|m| m.operator == "default_arg")
            .expect("should find default_arg mutation");
        assert_eq!(m.original, "0");
        assert_eq!(m.replacement, "1");
        // Offset correctness: the `0` default is at position 8 in "def f(x=0):\n    return x\n"
        assert_eq!(&fms[0].source[m.start..m.end], "0", "source slice must equal original");
    }

    #[test]
    fn test_default_none_to_empty_string() {
        let source = "def f(x=None):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0].mutations.iter().find(|m| m.operator == "default_arg")
            .expect("should find default_arg mutation");
        assert_eq!(m.original, "None");
        assert_eq!(m.replacement, "\"\"");
        assert_eq!(&fms[0].source[m.start..m.end], "None");
    }

    #[test]
    fn test_default_string_to_xx() {
        let source = "def f(x=\"hello\"):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0].mutations.iter().find(|m| m.operator == "default_arg")
            .expect("should find default_arg mutation");
        assert_eq!(m.original, "\"hello\"");
        assert_eq!(m.replacement, "\"XXhelloXX\"");
        assert_eq!(&fms[0].source[m.start..m.end], "\"hello\"");
    }

    #[test]
    fn test_default_bool_swapped() {
        let source = "def f(x=True):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0].mutations.iter().find(|m| m.operator == "default_arg")
            .expect("should find default_arg mutation");
        assert_eq!(m.original, "True");
        assert_eq!(m.replacement, "False");
        assert_eq!(&fms[0].source[m.start..m.end], "True");
    }

    #[test]
    fn test_no_default_no_mutation() {
        let source = "def f(x):\n    return x + 1\n";
        let fms = collect_file_mutations(source);
        let default_muts: Vec<_> = fms[0].mutations.iter()
            .filter(|m| m.operator == "default_arg")
            .collect();
        assert!(default_muts.is_empty(), "param without default should produce no default_arg mutation");
    }

    #[test]
    fn test_multiple_defaults_independent() {
        let source = "def f(x=0, y=1):\n    return x + y\n";
        let fms = collect_file_mutations(source);
        let default_muts: Vec<_> = fms[0].mutations.iter()
            .filter(|m| m.operator == "default_arg")
            .collect();
        assert_eq!(default_muts.len(), 2, "two params with defaults → two mutations");
        // x=0 → x=1
        let mx = default_muts.iter().find(|m| m.original == "0").expect("mutation for x=0");
        assert_eq!(mx.replacement, "1");
        assert_eq!(&fms[0].source[mx.start..mx.end], "0");
        // y=1 → y=2
        let my = default_muts.iter().find(|m| m.original == "1").expect("mutation for y=1");
        assert_eq!(my.replacement, "2");
        assert_eq!(&fms[0].source[my.start..my.end], "1");
    }

    #[test]
    fn test_default_arg_span_correctness() {
        // Verify that applying each default_arg mutation to func_source produces valid output.
        let sources = [
            "def f(x=0):\n    return x\n",
            "def f(x=None):\n    return x\n",
            "def f(x=\"hello\"):\n    return x\n",
            "def f(x=True):\n    return x\n",
            "def f(x=0, y=1):\n    return x + y\n",
        ];
        for source in sources {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "default_arg") {
                    // Span correctness: source[start..end] == original
                    assert_eq!(
                        &fm.source[m.start..m.end], m.original.as_str(),
                        "span mismatch for source: {source}"
                    );
                    // Replacement differs
                    assert_ne!(m.original, m.replacement, "replacement must differ");
                }
            }
        }
    }

    #[test]
    fn test_default_arg_parseable() {
        // After applying each default_arg mutation, the resulting function must parse as valid Python.
        let sources = [
            "def f(x=0):\n    return x\n",
            "def f(x=None):\n    return x\n",
            "def f(x=\"hello\"):\n    return x\n",
            "def f(x=True):\n    return x\n",
            "def f(x=False):\n    return x\n",
            "def f(x=3.14):\n    return x\n",
            "def f(x=0, y=1):\n    return x\n",
        ];
        for source in sources {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "default_arg") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "mutated source must parse as valid Python:\n{mutated}\n(original: {source})"
                    );
                }
            }
        }
    }

    // Kills mutant: line 329 `||` → `&&` (float detection via `.`) and
    //               line 332 `!=` → `==` (float dedup guard).
    // A simple float like `1.5` contains `.` but NOT `e`, so with `&&` it would
    // skip the float branch entirely — no default_arg mutation would be emitted.
    // With `==`, the dedup guard `r != trimmed` would flip to `r == trimmed`, which
    // is never true for n+1.0 vs n, so the mutation would be suppressed.
    #[test]
    fn test_default_float_simple() {
        let source = "def f(x=1.5):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("def f(x=1.5) must produce a default_arg mutation");
        assert_eq!(m.original, "1.5");
        assert_eq!(m.replacement, "2.5");
        assert_eq!(&fms[0].source[m.start..m.end], "1.5");
    }

    // Kills mutant: line 329 `||` → `&&` via the `e` branch.
    // `1e2` contains `e` but NOT `.`, so with `&&` the float branch would be skipped.
    #[test]
    fn test_default_float_scientific() {
        let source = "def f(x=1e2):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("def f(x=1e2) must produce a default_arg mutation");
        assert_eq!(m.original, "1e2");
        // 1e2 = 100.0, +1.0 = 101.0
        assert_eq!(m.replacement, "101");
        assert_eq!(&fms[0].source[m.start..m.end], "1e2");
    }

    // Kills mutant: line 343 `==` → `!=` (triple-quote detection: `quote_char == '"'`).
    // Flipping to `!=` would choose `'''` as the triple for a `"`-quoted string, so
    // `!rest.starts_with("'''")` would be true for `'hello'` but the wrong check runs.
    // More directly: single-quoted string `'hello'` must produce a mutation.
    #[test]
    fn test_default_single_quoted_string() {
        let source = "def f(x='hello'):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("def f(x='hello') must produce a default_arg mutation");
        assert_eq!(m.original, "'hello'");
        assert_eq!(m.replacement, "'XXhelloXX'");
        assert_eq!(&fms[0].source[m.start..m.end], "'hello'");
    }

    // Kills mutant: line 344 `&&` → `||` (compound guard weakening).
    // Triple-quoted `"""doc"""` falls through to the `None` fallback — replacement must be "None".
    // If either `&&` becomes `||`, the guard weakens: `!starts_with(triple) || ends_with(q)` is
    // true for `"""doc"""` (ends_with `"` is true), so it would enter the string branch and
    // produce `"""XXdocXX"""` instead. The test pins the replacement to "None".
    #[test]
    fn test_default_triple_quoted_fallback() {
        let source = "def f(x=\"\"\"doc\"\"\"):\n    return x\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("triple-quoted default must still produce a default_arg mutation via fallback");
        // Must fall back to None replacement, NOT wrap with XX (which would happen if && → ||)
        assert_eq!(
            m.replacement, "None",
            "triple-quoted string must get fallback 'None' replacement, not XX-wrapping"
        );
        assert_ne!(
            m.replacement, "\"\"\"XXdocXX\"\"\"",
            "triple-quoted string must not be XX-wrapped"
        );
    }

    // Kills mutant: line 344 second `&&` → `||` (ends_with guard).
    // Confirms both sides of the compound guard work independently.
    // - single-quoted `'hi'`: must produce XX-wrapped mutation (not None fallback)
    // - triple-quoted `'''hi'''`: must produce None fallback (not XX-wrapped)
    #[test]
    fn test_default_string_guard_compound() {
        // Normal single-quoted: must produce XX mutation
        let source_single = "def f(x='hi'):\n    return x\n";
        let fms = collect_file_mutations(source_single);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("single-quoted 'hi' must produce default_arg mutation");
        assert_eq!(m.replacement, "'XXhiXX'", "single-quoted must get XX-wrapped replacement");

        // Triple-quoted: must fall back to None, not produce XX-wrapping
        // Second `&&` → `||` makes condition: `(A && B) || C` where C=`len>=2` is always true,
        // so triple-quoted would enter the string branch and produce '''XXhiXX''' instead.
        let source_triple = "def f(x='''hi'''):\n    return x\n";
        let fms2 = collect_file_mutations(source_triple);
        let m2 = fms2[0]
            .mutations
            .iter()
            .find(|m| m.operator == "default_arg")
            .expect("triple-quoted '''hi''' must produce a default_arg mutation (fallback to None)");
        assert_eq!(
            m2.replacement, "None",
            "triple-quoted must get fallback 'None', not '''XXhiXX'''"
        );
    }
}

// --- Keyword swap tests (break→return, continue→break) ---
#[cfg(test)]
mod keyword_swap_tests {
    use super::*;

    // INV-1: `while True: break` → break is replaced with continue.
    #[test]
    fn test_break_to_continue() {
        let source = "def f():\n    while True:\n        break\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty(), "should collect mutations from function");
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert!(!kw.is_empty(), "break inside while should produce a keyword_swap mutation");
        let m = kw[0];
        assert_eq!(m.original, "break", "original must be 'break'");
        assert_eq!(m.replacement, "continue", "replacement must be 'continue'");
    }

    // INV-2: `for x in y: continue` → continue is replaced with break.
    #[test]
    fn test_continue_to_break() {
        let source = "def f(y):\n    for x in y:\n        continue\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert!(!kw.is_empty(), "continue inside for should produce a keyword_swap mutation");
        let m = kw[0];
        assert_eq!(m.original, "continue", "original must be 'continue'");
        assert_eq!(m.replacement, "break", "replacement must be 'break'");
    }

    // INV-3: `break` inside nested if is still found.
    #[test]
    fn test_break_inside_nested_if() {
        let source = "def f(cond):\n    while True:\n        if cond:\n            break\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert!(!kw.is_empty(), "break inside nested if should still produce keyword_swap");
        assert_eq!(kw[0].original, "break");
        assert_eq!(kw[0].replacement, "continue");
    }

    // INV-3: Loop with both break and continue generates 2 keyword_swap mutations.
    #[test]
    fn test_break_and_continue_both_swapped() {
        let source = "def f(items, cond):\n    for x in items:\n        if cond:\n            break\n        else:\n            continue\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert_eq!(kw.len(), 2, "loop with break and continue must produce 2 keyword_swap mutations");
        let originals: Vec<&str> = kw.iter().map(|m| m.original.as_str()).collect();
        assert!(originals.contains(&"break"), "must have break mutation");
        assert!(originals.contains(&"continue"), "must have continue mutation");
        for m in &kw {
            if m.original == "break" {
                assert_eq!(m.replacement, "continue");
            } else {
                assert_eq!(m.replacement, "break");
            }
        }
    }

    // INV-3: Nested loops generate keyword_swap mutations at each nesting level independently.
    #[test]
    fn test_break_continue_nested_loops() {
        let source = "def f(outer, inner):\n    for x in outer:\n        break\n        for y in inner:\n            continue\n";
        let fms = collect_file_mutations(source);
        assert!(!fms.is_empty());
        let fm = &fms[0];
        let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
        assert_eq!(kw.len(), 2, "nested loops with break and continue must produce 2 keyword_swap mutations");
        // Verify each is at a distinct position
        assert_ne!(kw[0].start, kw[1].start, "break and continue must be at distinct positions");
    }

    // INV-4: All keyword_swap mutations produce valid Python (parse_module succeeds).
    #[test]
    fn test_keyword_swap_parseable() {
        let sources = [
            "def f():\n    while True:\n        break\n",
            "def f(y):\n    for x in y:\n        continue\n",
            "def f(cond):\n    while True:\n        if cond:\n            break\n",
        ];
        for source in &sources {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "keyword_swap") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "keyword_swap mutation {:?} → {:?} produced unparseable Python:\n{}",
                        m.original, m.replacement, mutated
                    );
                }
            }
        }
    }

    // INV-5: Mutation start/end match the keyword position in source.
    #[test]
    fn test_keyword_swap_span_correctness() {
        let cases = [
            ("def f():\n    while True:\n        break\n", "break"),
            ("def f(y):\n    for x in y:\n        continue\n", "continue"),
        ];
        for (source, keyword) in &cases {
            let fms = collect_file_mutations(source);
            assert!(!fms.is_empty());
            let fm = &fms[0];
            let kw: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "keyword_swap").collect();
            assert!(!kw.is_empty(), "expected keyword_swap for '{keyword}'");
            let m = kw[0];
            // start..end must index the keyword in the function source
            assert_eq!(
                &fm.source[m.start..m.end], *keyword,
                "source[{}..{}] must equal '{keyword}'",
                m.start, m.end
            );
            // start < end invariant
            assert!(m.start < m.end, "start must be < end");
            // end in bounds
            assert!(m.end <= fm.source.len(), "end must be <= source length");
        }
    }
}

#[cfg(test)]
mod return_value_tests {
    use super::*;

    fn return_value_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations.into_iter())
            .filter(|m| m.operator == "return_value")
            .collect()
    }

    // INV-1: `return a + b` → mutation replaces "a + b" with "None"
    #[test]
    fn test_return_expr_to_none() {
        let source = "def f(a, b):\n    return a + b\n";
        let muts = return_value_mutations(source);
        assert_eq!(muts.len(), 1, "return expr must produce exactly 1 return_value mutation");
        assert_eq!(muts[0].replacement, "None");
        assert_eq!(muts[0].original, "a + b");
    }

    // INV-2: `return None` → mutation replaces "None" with `""`
    #[test]
    fn test_return_none_to_empty_string() {
        let source = "def f():\n    return None\n";
        let muts = return_value_mutations(source);
        assert_eq!(muts.len(), 1, "return None must produce exactly 1 return_value mutation");
        assert_eq!(muts[0].replacement, "\"\"");
        assert_eq!(muts[0].original, "None");
    }

    // INV-3: `return 42` → mutation replaces "42" with "None"
    #[test]
    fn test_return_constant_to_none() {
        let source = "def f():\n    return 42\n";
        let muts = return_value_mutations(source);
        assert_eq!(muts.len(), 1, "return 42 must produce exactly 1 return_value mutation");
        assert_eq!(muts[0].replacement, "None");
        assert_eq!(muts[0].original, "42");
    }

    // INV-4: `return "hello"` → mutation replaces `"hello"` with "None"
    #[test]
    fn test_return_string_to_none() {
        let source = "def f():\n    return \"hello\"\n";
        let muts = return_value_mutations(source);
        assert_eq!(muts.len(), 1, "return string must produce exactly 1 return_value mutation");
        assert_eq!(muts[0].replacement, "None");
        assert_eq!(muts[0].original, "\"hello\"");
    }

    // INV-5: bare `return` (no value) → no return_value mutation
    #[test]
    fn test_bare_return_no_mutation() {
        // bare return needs something else to produce a mutation so the function is collected
        let source = "def f(a, b):\n    if a > b:\n        return\n    return a + b\n";
        let muts = return_value_mutations(source);
        // Should get exactly one return_value mutation (from `return a + b`), not from bare `return`
        assert_eq!(muts.len(), 1, "bare return must not emit a return_value mutation");
        assert_eq!(muts[0].original, "a + b");
    }

    // INV-6: `return a + b` → produces BOTH return_value AND binop_swap mutations
    #[test]
    fn test_return_value_coexists_with_binop() {
        let source = "def f(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        let rv: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "return_value").collect();
        let binop: Vec<_> = fm.mutations.iter().filter(|m| m.operator == "binop_swap").collect();
        assert!(!rv.is_empty(), "must have return_value mutation");
        assert!(!binop.is_empty(), "must also have binop_swap mutation");
    }

    // INV-7: All return_value mutations produce syntactically valid Python
    #[test]
    fn test_return_value_parseable() {
        let cases = [
            "def f(a, b):\n    return a + b\n",
            "def f():\n    return None\n",
            "def f():\n    return 42\n",
            "def f():\n    return \"hello\"\n",
            "def f(a, b):\n    return a and b\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "return_value") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "return_value mutation '{}' → '{}' produced unparseable Python:\n{}",
                        m.original,
                        m.replacement,
                        mutated
                    );
                }
            }
        }
    }

    // INV-8: Mutation span covers only the value, not the `return` keyword
    #[test]
    fn test_return_value_span_correctness() {
        let source = "def f(a, b):\n    return a + b\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        let m = fm.mutations.iter().find(|m| m.operator == "return_value").unwrap();

        // The span text must equal the original
        assert_eq!(&fm.source[m.start..m.end], m.original.as_str());

        // The span must NOT include "return"
        let before_span = &fm.source[..m.start];
        assert!(
            before_span.ends_with("return "),
            "the text before the value span must end with 'return ', got: {:?}",
            before_span
        );
    }

    // INV-9: `return 42` emits a return_value mutation replacing "42" with "None".
    // This directly exercises the guard: if the condition flips from `>` to `<`,
    // add_return_value_mutation called with ret_text that has trailing content would
    // incorrectly suppress valid mutations.
    #[test]
    fn test_return_value_guard_emits_mutation() {
        // Direct unit test of add_return_value_mutation with trailing whitespace in ret_text.
        // val_offset(7) + val_text.len(2) = 9 < ret_text.len(10)
        // With correct guard `>`: 9 > 10 = false → mutation IS emitted
        // With mutant guard `<`: 9 < 10 = true → mutation suppressed (bug!)
        let mut mutations = Vec::new();
        add_return_value_mutation("42", "return 42 ", 0, &mut mutations);
        assert_eq!(mutations.len(), 1, "guard must not suppress valid mutation when ret_text has trailing content");
        assert_eq!(mutations[0].replacement, "None");
        assert_eq!(mutations[0].original, "42");
    }

    // INV-10: `return None` emits a return_value mutation replacing "None" with `""`.
    // Same guard boundary test for the None→"" path.
    #[test]
    fn test_return_value_none_emits_empty_string() {
        // val_offset(7) + val_text.len(4) = 11 < ret_text.len(12)
        // With correct guard `>`: 11 > 12 = false → mutation IS emitted
        // With mutant guard `<`: 11 < 12 = true → mutation suppressed (bug!)
        let mut mutations = Vec::new();
        add_return_value_mutation("None", "return None ", 0, &mut mutations);
        assert_eq!(mutations.len(), 1, "None→\"\" mutation must be emitted even when ret_text has trailing content");
        assert_eq!(mutations[0].replacement, "\"\"");
        assert_eq!(mutations[0].original, "None");
    }

    // =====================================================================
    // Decorator removal tests
    // =====================================================================
    #[cfg(test)]
    mod decorator_removal_tests {
        use super::*;

        // INV-1: A single non-skipped decorator produces exactly one decorator_removal mutation.
        #[test]
        fn test_single_decorator_removed() {
            let source = "@cache\ndef f():\n    return 1\n";
            let fms = collect_file_mutations(source);
            assert_eq!(fms.len(), 1, "decorated function must be collected");
            let removals: Vec<_> = fms[0]
                .mutations
                .iter()
                .filter(|m| m.operator == "decorator_removal")
                .collect();
            assert_eq!(removals.len(), 1, "should produce one decorator_removal");
            assert_eq!(removals[0].original, "@cache\n", "original must be the full decorator line");
            assert_eq!(removals[0].replacement, "", "replacement must be empty string");
        }

        // INV-2: Two non-skipped decorators each produce an independent decorator_removal mutation.
        #[test]
        fn test_multiple_decorators_independent() {
            let source = "@a\n@b\ndef f():\n    return 1\n";
            let fms = collect_file_mutations(source);
            assert_eq!(fms.len(), 1);
            let removals: Vec<_> = fms[0]
                .mutations
                .iter()
                .filter(|m| m.operator == "decorator_removal")
                .collect();
            assert_eq!(removals.len(), 2, "should produce two decorator_removal mutations");
            // Spans must be distinct and non-overlapping
            let start_a = removals.iter().find(|m| m.original == "@a\n").expect("@a\n removal");
            let start_b = removals.iter().find(|m| m.original == "@b\n").expect("@b\n removal");
            assert!(
                start_a.start != start_b.start,
                "different decorators must have different start offsets"
            );
        }

        // INV-3: @abstractmethod is never emitted as a decorator_removal mutation.
        #[test]
        fn test_abstractmethod_skipped() {
            let source = "@abstractmethod\ndef f():\n    return 1\n";
            let fms = collect_file_mutations(source);
            // Only body mutations (return_value) — no decorator_removal.
            if let Some(fm) = fms.first() {
                let removals: Vec<_> = fm
                    .mutations
                    .iter()
                    .filter(|m| m.operator == "decorator_removal")
                    .collect();
                assert!(removals.is_empty(), "@abstractmethod must not be removed");
            }
        }

        // INV-4: @override is never emitted as a decorator_removal mutation.
        #[test]
        fn test_override_skipped() {
            let source = "@override\ndef f():\n    return 1\n";
            let fms = collect_file_mutations(source);
            if let Some(fm) = fms.first() {
                let removals: Vec<_> = fm
                    .mutations
                    .iter()
                    .filter(|m| m.operator == "decorator_removal")
                    .collect();
                assert!(removals.is_empty(), "@override must not be removed");
            }
        }

        // INV-5: Decorator with arguments produces one removal mutation for the full call line.
        #[test]
        fn test_decorator_with_args() {
            let source = "@app.route(\"/path\")\ndef f():\n    return 1\n";
            let fms = collect_file_mutations(source);
            assert_eq!(fms.len(), 1);
            let removals: Vec<_> = fms[0]
                .mutations
                .iter()
                .filter(|m| m.operator == "decorator_removal")
                .collect();
            assert_eq!(removals.len(), 1, "decorated function with args must produce one removal");
            assert!(
                removals[0].original.starts_with("@app.route"),
                "original must include the full decorator call"
            );
            assert_eq!(removals[0].replacement, "");
        }

        // INV-6: Applying any decorator_removal mutation produces parseable Python.
        #[test]
        fn test_decorator_removal_parseable() {
            let cases = [
                "@cache\ndef f():\n    return 1\n",
                "@a\n@b\ndef f():\n    return 1\n",
                "@app.route(\"/path\")\ndef f():\n    return 1\n",
                "@staticmethod\ndef f():\n    return 1\n",
            ];
            for source in &cases {
                let fms = collect_file_mutations(source);
                for fm in &fms {
                    for m in fm.mutations.iter().filter(|m| m.operator == "decorator_removal") {
                        let mutated = apply_mutation(&fm.source, m);
                        assert!(
                            parse_module(&mutated, None).is_ok(),
                            "decorator_removal of '{}' produced unparseable Python:\n{}",
                            m.original,
                            mutated
                        );
                    }
                }
            }
        }

        // INV-7: The decorator_removal mutation span correctly covers the full decorator line.
        // source[m.start..m.end] == m.original, and m.start < m.end.
        #[test]
        fn test_decorator_removal_span_correctness() {
            let cases = [
                "@cache\ndef f():\n    return 1\n",
                "@a\n@b\ndef f():\n    return 1\n",
                "@staticmethod\ndef f(self):\n    return 1\n",
            ];
            for source in &cases {
                let fms = collect_file_mutations(source);
                for fm in &fms {
                    for m in fm.mutations.iter().filter(|m| m.operator == "decorator_removal") {
                        // Span bounds must be valid
                        assert!(m.start < m.end, "start < end must hold");
                        assert!(m.end <= fm.source.len(), "end must be within source");
                        // Source slice must equal original
                        assert_eq!(
                            &fm.source[m.start..m.end],
                            m.original.as_str(),
                            "source[{}..{}] must equal original for decorator in {:?}",
                            m.start,
                            m.end,
                            source
                        );
                    }
                }
            }
        }

        // INV-8: A decorated function also collects body mutations (e.g. binop_swap).
        #[test]
        fn test_decorated_function_body_mutations_collected() {
            let source = "@cache\ndef f(a, b):\n    return a + b\n";
            let fms = collect_file_mutations(source);
            assert_eq!(fms.len(), 1);
            let body_muts: Vec<_> = fms[0]
                .mutations
                .iter()
                .filter(|m| m.operator != "decorator_removal")
                .collect();
            assert!(!body_muts.is_empty(), "body mutations must also be collected");
        }
    }
}

// --- Conditional split/rsplit method swap tests ---
#[cfg(test)]
mod split_swap_tests {
    use super::*;

    #[test]
    fn test_split_with_maxsplit_mutated() {
        let source = "def foo(s):\n    return s.split(\",\", 1)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("split with 2 positional args must produce a method_swap mutation");
        assert_eq!(m.original, "split");
        assert_eq!(m.replacement, "rsplit");
    }

    #[test]
    fn test_rsplit_with_maxsplit_mutated() {
        let source = "def foo(s):\n    return s.rsplit(\",\", 1)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("rsplit with 2 positional args must produce a method_swap mutation");
        assert_eq!(m.original, "rsplit");
        assert_eq!(m.replacement, "split");
    }

    #[test]
    fn test_split_with_maxsplit_kwarg() {
        let source = "def foo(s):\n    return s.split(\",\", maxsplit=1)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("split with maxsplit kwarg must produce a method_swap mutation");
        assert_eq!(m.original, "split");
        assert_eq!(m.replacement, "rsplit");
    }

    #[test]
    fn test_rsplit_with_maxsplit_kwarg() {
        let source = "def foo(s):\n    return s.rsplit(\",\", maxsplit=1)\n";
        let fms = collect_file_mutations(source);
        let m = fms[0]
            .mutations
            .iter()
            .find(|m| m.operator == "method_swap")
            .expect("rsplit with maxsplit kwarg must produce a method_swap mutation");
        assert_eq!(m.original, "rsplit");
        assert_eq!(m.replacement, "split");
    }

    // INV: split/rsplit with exactly 1 positional arg and no maxsplit kwarg must NOT produce
    // a method_swap mutation — without maxsplit the two calls are semantically identical.
    #[test]
    fn test_split_one_arg_not_mutated() {
        let source = "def foo(s):\n    return s.split(\",\")\n";
        let fms = collect_file_mutations(source);
        let method_muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "method_swap")
            .collect();
        assert!(method_muts.is_empty(), "split with 1 arg must not produce a method_swap mutation");
    }

    #[test]
    fn test_split_no_args_not_mutated() {
        let source = "def foo(s):\n    return s.split()\n";
        let fms = collect_file_mutations(source);
        let method_muts: Vec<_> = fms
            .iter()
            .flat_map(|fm| fm.mutations.iter())
            .filter(|m| m.operator == "method_swap")
            .collect();
        assert!(method_muts.is_empty(), "split with no args must not produce a method_swap mutation");
    }

    // INV: split/rsplit mutation span is structurally correct — character before start is '.'.
    #[test]
    fn test_split_swap_span_correctness() {
        let cases = [
            "def foo(s):\n    return s.split(\",\", 1)\n",
            "def foo(s):\n    return s.rsplit(\",\", 1)\n",
            "def foo(s):\n    return s.split(\",\", maxsplit=1)\n",
        ];
        for source in &cases {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in &fm.mutations {
                    if m.operator == "method_swap" && (m.original == "split" || m.original == "rsplit") {
                        assert_eq!(
                            &fm.source[m.start..m.end],
                            m.original,
                            "span must cover the method name in {:?}",
                            source
                        );
                        assert!(m.start > 0, "method_swap start must be > 0");
                        assert_eq!(
                            fm.source.as_bytes()[m.start - 1],
                            b'.',
                            "character before method span must be a dot in {:?}",
                            source
                        );
                    }
                }
            }
        }
    }

    // INV: apply_mutation on a split/rsplit swap produces syntactically valid Python
    // (i.e., only the method name changes, all parens and args are preserved).
    #[test]
    fn test_split_swap_parseable() {
        let cases = [
            ("def foo(s):\n    return s.split(\",\", 1)\n", "split", "rsplit"),
            ("def foo(s):\n    return s.rsplit(\",\", 1)\n", "rsplit", "split"),
        ];
        for (source, original, replacement) in &cases {
            let fms = collect_file_mutations(source);
            let m = fms[0]
                .mutations
                .iter()
                .find(|m| m.operator == "method_swap" && m.original == *original)
                .expect("must find method_swap mutation");
            let mutated = apply_mutation(&fms[0].source, m);
            assert!(
                mutated.contains(replacement),
                "mutated source must contain replacement method name {:?}: got {:?}",
                replacement,
                mutated
            );
            // The parens and arguments must still be present.
            assert!(
                mutated.contains("(\",\", 1)"),
                "mutated source must preserve call arguments: got {:?}",
                mutated
            );
        }
    }
}

#[cfg(test)]
mod dict_kwarg_tests {
    use super::*;

    fn kwarg_mutations(source: &str) -> Vec<Mutation> {
        let fms = collect_file_mutations(source);
        fms.into_iter()
            .flat_map(|fm| fm.mutations)
            .filter(|m| m.operator == "dict_kwarg")
            .collect()
    }

    #[test]
    fn test_dict_single_kwarg() {
        let source = "def f():\n    return dict(a=1)\n";
        let muts = kwarg_mutations(source);
        assert_eq!(muts.len(), 1, "dict(a=1) must produce exactly one dict_kwarg mutation");
        assert_eq!(muts[0].original, "a");
        assert_eq!(muts[0].replacement, "aXX");
    }

    #[test]
    fn test_dict_multiple_kwargs() {
        let source = "def f():\n    return dict(a=1, b=2)\n";
        let muts = kwarg_mutations(source);
        assert_eq!(muts.len(), 2, "dict(a=1, b=2) must produce two dict_kwarg mutations");
        let originals: Vec<&str> = muts.iter().map(|m| m.original.as_str()).collect();
        assert!(originals.contains(&"a"), "must mutate kwarg 'a'");
        assert!(originals.contains(&"b"), "must mutate kwarg 'b'");
        assert_eq!(muts.iter().find(|m| m.original == "a").unwrap().replacement, "aXX");
        assert_eq!(muts.iter().find(|m| m.original == "b").unwrap().replacement, "bXX");
    }

    #[test]
    fn test_dict_no_kwargs() {
        let source = "def f():\n    return dict()\n";
        let muts = kwarg_mutations(source);
        assert!(muts.is_empty(), "dict() must produce no dict_kwarg mutations");
    }

    #[test]
    fn test_dict_positional_only() {
        let source = "def f():\n    return dict([(1, 2)])\n";
        let muts = kwarg_mutations(source);
        assert!(muts.is_empty(), "dict with positional-only args must not produce dict_kwarg mutations");
    }

    #[test]
    fn test_dict_mixed_args() {
        // dict(a=1, **extra) — only `a` is a plain keyword arg; **extra is starred
        let source = "def f(extra):\n    return dict(a=1, **extra)\n";
        let muts = kwarg_mutations(source);
        assert_eq!(muts.len(), 1, "only plain kwarg 'a' must be mutated, not **extra");
        assert_eq!(muts[0].original, "a");
        assert_eq!(muts[0].replacement, "aXX");
    }

    #[test]
    fn test_non_dict_call_no_mutation() {
        // foo(a=1) must NOT produce dict_kwarg mutations — only dict() calls are targeted.
        let source = "def f():\n    foo(a=1)\n";
        let muts = kwarg_mutations(source);
        assert!(muts.is_empty(), "foo(a=1) must not produce dict_kwarg mutations");
    }

    #[test]
    fn test_dict_kwarg_parseable() {
        // Verify that applying all dict_kwarg mutations produces valid (parseable) Python.
        let source = "def f():\n    return dict(foo=1, bar=2)\n";
        let fms = collect_file_mutations(source);
        for fm in &fms {
            for m in fm.mutations.iter().filter(|m| m.operator == "dict_kwarg") {
                let mutated = apply_mutation(&fm.source, m);
                // A mutated source is parseable if libcst can collect mutations from it.
                // We only need to verify that collect_file_mutations doesn't panic.
                let _ = collect_file_mutations(&mutated);
            }
        }
    }

    #[test]
    fn test_dict_kwarg_span_correctness() {
        // INV-3: fm.source[m.start..m.end] must equal m.original for dict_kwarg mutations.
        let source = "def f():\n    return dict(foo=1, bar=2)\n";
        let fms = collect_file_mutations(source);
        for fm in &fms {
            for m in fm.mutations.iter().filter(|m| m.operator == "dict_kwarg") {
                let slice = &fm.source[m.start..m.end];
                assert_eq!(
                    slice, m.original.as_str(),
                    "source slice at [{}..{}] must equal original '{}', got '{}'",
                    m.start, m.end, m.original, slice
                );
            }
        }
    }
}

#[cfg(test)]
mod exception_type_tests {
    use super::*;

    fn exception_type_mutations_for(source: &str) -> Vec<(FunctionMutations, Mutation)> {
        collect_file_mutations(source)
            .into_iter()
            .flat_map(|fm| {
                let pairs: Vec<_> = fm
                    .mutations
                    .iter()
                    .filter(|m| m.operator == "exception_type")
                    .map(|m| (fm.clone(), m.clone()))
                    .collect();
                pairs
            })
            .collect()
    }

    #[test]
    fn test_except_valueerror_to_exception() {
        let source = "def f():\n    try:\n        pass\n    except ValueError:\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one exception_type mutation expected");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "ValueError");
        assert_eq!(m.replacement, "Exception");
        assert_eq!(&fm.source[m.start..m.end], "ValueError");
    }

    #[test]
    fn test_except_tuple_to_exception() {
        let source =
            "def f():\n    try:\n        pass\n    except (TypeError, ValueError):\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one exception_type mutation expected for tuple type");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "(TypeError, ValueError)");
        assert_eq!(m.replacement, "Exception");
        assert_eq!(&fm.source[m.start..m.end], "(TypeError, ValueError)");
    }

    #[test]
    fn test_except_exception_no_mutation() {
        // `except Exception:` is already the broadest type — no mutation should be emitted.
        let source = "def f():\n    try:\n        pass\n    except Exception:\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 0, "except Exception must not produce an exception_type mutation");
    }

    #[test]
    fn test_bare_except_no_mutation() {
        // Bare `except:` has no type field — nothing to broaden.
        let source = "def f():\n    try:\n        pass\n    except:\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 0, "bare except must not produce an exception_type mutation");
    }

    #[test]
    fn test_except_with_as_binding() {
        // `except ValueError as e:` — mutation targets only the type, not the `as e` binding.
        let source =
            "def f():\n    try:\n        pass\n    except ValueError as e:\n        pass\n";
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one exception_type mutation expected");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "ValueError");
        assert_eq!(m.replacement, "Exception");
        assert_eq!(&fm.source[m.start..m.end], "ValueError");
        // The character immediately after the type span must be a space (before `as`).
        assert_eq!(
            fm.source.as_bytes()[m.end],
            b' ',
            "char after type span must be space (before 'as')"
        );
    }

    #[test]
    fn test_multiple_handlers() {
        // One mutation per typed handler; both TypeError and ValueError should be mutated.
        let source = concat!(
            "def f():\n",
            "    try:\n",
            "        pass\n",
            "    except TypeError:\n",
            "        pass\n",
            "    except ValueError:\n",
            "        pass\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 2, "two typed handlers must produce two exception_type mutations");
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"TypeError"), "TypeError handler must be mutated");
        assert!(originals.contains(&"ValueError"), "ValueError handler must be mutated");
    }

    #[test]
    fn test_exception_type_parseable() {
        // After mutation, the function source must still parse as valid Python.
        let source =
            "def f():\n    try:\n        pass\n    except ValueError:\n        pass\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        let exc_m = fm
            .mutations
            .iter()
            .find(|m| m.operator == "exception_type")
            .expect("must have an exception_type mutation");
        let mutated = apply_mutation(&fm.source, exc_m);
        assert!(
            parse_module(&mutated, None).is_ok(),
            "mutated source must be parseable: {mutated}"
        );
    }

    #[test]
    fn test_exception_type_span_correctness() {
        // INV-3: fm.source[m.start..m.end] must equal m.original for exception_type mutations.
        let source =
            "def f():\n    try:\n        pass\n    except ValueError:\n        pass\n";
        let fms = collect_file_mutations(source);
        assert_eq!(fms.len(), 1);
        let fm = &fms[0];
        let exc_m = fm
            .mutations
            .iter()
            .find(|m| m.operator == "exception_type")
            .expect("must have an exception_type mutation");
        assert_eq!(
            &fm.source[exc_m.start..exc_m.end],
            exc_m.original.as_str(),
            "source slice must equal mutation original"
        );
    }

    #[test]
    fn test_bare_then_typed_handler() {
        // Bare except in one try block, typed except in a separate try block.
        // The bare except cursor advance must not discard the typed handler in the second block.
        // Since each try block calls add_exception_type_mutations independently, the cursor for
        // the second block (cursor_before_2) is derived from the structural cursor after the
        // first block. Exactly 1 exception_type mutation (on ValueError) must be emitted.
        let source = concat!(
            "def f(x):\n",
            "    try:\n",
            "        return x + 1\n",
            "    except:\n",
            "        pass\n",
            "    try:\n",
            "        return x + 2\n",
            "    except ValueError:\n",
            "        return 0\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 1, "exactly one exception_type mutation expected (from the second try block)");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "ValueError");
        assert_eq!(&fm.source[m.start..m.end], "ValueError");
    }

    #[test]
    fn test_two_typed_handlers() {
        // Two typed handlers in the same try block — one for ValueError, one for TypeError.
        // Each must produce an independent exception_type mutation.
        let source = concat!(
            "def f(x):\n",
            "    try:\n",
            "        return x + 1\n",
            "    except ValueError:\n",
            "        return 0\n",
            "    except TypeError:\n",
            "        return -1\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 2, "exactly two exception_type mutations expected");
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"ValueError"), "ValueError must be mutated");
        assert!(originals.contains(&"TypeError"), "TypeError must be mutated");
        // Each mutation must point to a distinct position in the source.
        assert_ne!(pairs[0].1.start, pairs[1].1.start, "mutations must target different source positions");
        for (fm, m) in &pairs {
            assert_eq!(&fm.source[m.start..m.end], m.original.as_str(), "span must match original");
        }
    }

    #[test]
    fn test_three_handlers_mixed() {
        // Three typed handlers — ValueError, TypeError, Exception.
        // Exception is already the broadest type and must be skipped.
        // Exactly 2 exception_type mutations must be emitted (for ValueError and TypeError).
        let source = concat!(
            "def f(x):\n",
            "    try:\n",
            "        return x\n",
            "    except ValueError:\n",
            "        return 1\n",
            "    except TypeError:\n",
            "        return 2\n",
            "    except Exception:\n",
            "        return 3\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 2, "Exception handler must be skipped; exactly 2 mutations expected");
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"ValueError"), "ValueError must be mutated");
        assert!(originals.contains(&"TypeError"), "TypeError must be mutated");
        assert!(!originals.contains(&"Exception"), "Exception must not be mutated");
        // Mutations must target distinct, increasing positions (cursor advances forward).
        assert!(
            pairs[0].1.start < pairs[1].1.start,
            "mutations must be ordered by source position"
        );
        for (fm, m) in &pairs {
            assert_eq!(&fm.source[m.start..m.end], m.original.as_str(), "span must match original");
        }
    }

    #[test]
    fn test_duplicate_handlers_distinct_positions() {
        // Two handlers of the same exception type in the same try block.
        // Python allows this (the second is unreachable); libcst parses it fine.
        // The sub-cursor must advance PAST the first handler before searching for the second,
        // so both mutations must point to distinct positions.
        // Regression: if cursor goes backward after the first handler, it re-finds the first
        // handler for the second — both mutations collapse to the same span.
        let source = concat!(
            "def f(x):\n",
            "    try:\n",
            "        return x + 1\n",
            "    except ValueError:\n",
            "        return 0\n",
            "    except ValueError:\n",
            "        return -1\n",
        );
        let pairs = exception_type_mutations_for(source);
        assert_eq!(pairs.len(), 2, "two exception_type mutations expected (one per handler)");
        // The two mutations must point to different byte offsets in the source.
        assert_ne!(
            pairs[0].1.start,
            pairs[1].1.start,
            "cursor must advance past first handler before searching for second (distinct positions required)"
        );
        for (fm, m) in &pairs {
            assert_eq!(&fm.source[m.start..m.end], m.original.as_str(), "span must match original");
        }
    }

    // --- condition_negation tests ---

    // INV-1: Applying any condition_negation mutation must produce parseable Python.
    // INV-2: Operator name is always "condition_negation".
    // INV-3: Replacement is always `not ({original_condition})`.

    fn condition_negation_mutations_for(source: &str) -> Vec<(FunctionMutations, Mutation)> {
        collect_file_mutations(source)
            .into_iter()
            .flat_map(|fm| {
                fm.mutations
                    .iter()
                    .filter(|m| m.operator == "condition_negation")
                    .cloned()
                    .map(|m| (fm.clone(), m))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn test_condition_negation_if_statement() {
        // Critical path: `if x > 0:` → `if not (x > 0):`
        let source = "def f(x):\n    if x > 0:\n        return x\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one condition_negation for a single if");
        let (fm, m) = &pairs[0];
        // INV-2
        assert_eq!(m.operator, "condition_negation");
        // INV-3: replacement must wrap original in `not (...)`
        assert_eq!(m.original, "x > 0");
        assert_eq!(m.replacement, "not (x > 0)");
        // INV-1: mutated source must parse
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_while_loop() {
        // Critical path: `while items:` generates one mutation.
        let source = "def f(items):\n    while items:\n        items.pop()\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one condition_negation for a single while");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "items");
        assert_eq!(m.replacement, "not (items)");
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_assert_no_message() {
        // Critical path: `assert result == expected` → condition mutated, no msg.
        let source = "def f(result, expected):\n    assert result == expected\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one condition_negation for assert without message");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "result == expected");
        assert_eq!(m.replacement, "not (result == expected)");
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_assert_with_message() {
        // Critical path: `assert cond, "msg"` — mutation targets condition only, message preserved.
        let source = "def f(result):\n    assert result, \"expected true\"\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one condition_negation for assert with message");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "result");
        assert_eq!(m.replacement, "not (result)");
        // The mutated function must still include the message.
        let mutated = apply_mutation(&fm.source, m);
        assert!(mutated.contains("\"expected true\""), "message must be preserved in mutated source");
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_ternary_expression() {
        // Critical path: `x if flag else y` → `x if not (flag) else y`
        let source = "def f(x, y, flag):\n    return x if flag else y\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 1, "one condition_negation for ternary");
        let (fm, m) = &pairs[0];
        assert_eq!(m.original, "flag");
        assert_eq!(m.replacement, "not (flag)");
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_compound_condition() {
        // Compound: `if a and b or c:` → `if not (a and b or c):`
        let source = "def f(a, b, c):\n    if a and b or c:\n        return 1\n";
        let pairs = condition_negation_mutations_for(source);
        let cn: Vec<_> = pairs.iter().filter(|(_, m)| m.operator == "condition_negation").collect();
        assert_eq!(cn.len(), 1, "one condition_negation for compound condition");
        let (fm, m) = &cn[0];
        assert_eq!(m.replacement, format!("not ({})", m.original));
        let mutated = apply_mutation(&fm.source, m);
        assert!(parse_module(&mutated, None).is_ok(), "mutated source must be parseable: {mutated}");
    }

    #[test]
    fn test_condition_negation_already_negated_skipped() {
        // Failure mode: `if not x:` must NOT generate condition_negation (unary_removal covers it).
        let source = "def f(x):\n    if not x:\n        return 1\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 0, "condition_negation must be skipped when condition is already `not <expr>`");
    }

    #[test]
    fn test_condition_negation_elif_branch() {
        // `elif` branches must also get condition_negation mutations.
        let source = "def f(x):\n    if x > 0:\n        return 1\n    elif x < 0:\n        return -1\n    else:\n        return 0\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 2, "if and elif each get one condition_negation");
        // Both conditions appear in the mutations
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"x > 0"), "if condition must be mutated");
        assert!(originals.contains(&"x < 0"), "elif condition must be mutated");
        for (fm, m) in &pairs {
            let mutated = apply_mutation(&fm.source, m);
            assert!(parse_module(&mutated, None).is_ok(), "mutated source must parse: {mutated}");
        }
    }

    #[test]
    fn test_condition_negation_nested_if() {
        // Nested if inside if — both conditions get independent mutations.
        let source = "def f(a, b):\n    if a:\n        if b:\n            return 1\n";
        let pairs = condition_negation_mutations_for(source);
        assert_eq!(pairs.len(), 2, "outer and inner if each get one condition_negation");
        let originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        assert!(originals.contains(&"a"), "outer if condition must be mutated");
        assert!(originals.contains(&"b"), "inner if condition must be mutated");
        for (fm, m) in &pairs {
            let mutated = apply_mutation(&fm.source, m);
            assert!(parse_module(&mutated, None).is_ok(), "mutated source must parse: {mutated}");
        }
    }

    #[test]
    fn test_condition_negation_parseability_all_sites() {
        // INV-1: every condition_negation mutation must produce parseable Python across all sites.
        let cases = [
            "def f(x):\n    if x > 0:\n        return x\n",
            "def f(items):\n    while items:\n        items.pop()\n",
            "def f(r, e):\n    assert r == e\n",
            "def f(r):\n    assert r, \"msg\"\n",
            "def f(x, y, flag):\n    return x if flag else y\n",
            "def f(a, b, c):\n    if a and b or c:\n        return 1\n",
        ];
        for source in &cases {
            let pairs = condition_negation_mutations_for(source);
            assert!(!pairs.is_empty(), "should produce at least one condition_negation for: {source}");
            for (fm, m) in &pairs {
                assert_eq!(m.operator, "condition_negation");
                assert_eq!(m.replacement, format!("not ({})", m.original), "INV-3 violated");
                let mutated = apply_mutation(&fm.source, m);
                assert!(
                    parse_module(&mutated, None).is_ok(),
                    "INV-1 violated: unparseable mutant for {source}: {mutated}"
                );
            }
        }
    }
}

#[cfg(test)]
mod ternary_swap_tests {
    use super::*;
    use libcst_native::parse_module;

    fn ternary_mutations(source: &str) -> Vec<(FunctionMutations, Mutation)> {
        collect_file_mutations(source)
            .into_iter()
            .flat_map(|fm| {
                fm.mutations
                    .iter()
                    .filter(|m| m.operator == "ternary_swap")
                    .map(|m| (fm.clone(), m.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    // INV-2: operator name is "ternary_swap"
    #[test]
    fn test_operator_name() {
        let source = "def f(flag):\n    return x if flag else y\n";
        let pairs = ternary_mutations(source);
        assert!(!pairs.is_empty(), "must produce at least one ternary_swap mutation");
        for (_, m) in &pairs {
            assert_eq!(m.operator, "ternary_swap");
        }
    }

    // INV-3: condition is preserved; only body and orelse are swapped
    #[test]
    fn test_simple_swap() {
        let source = "def f(flag):\n    return x if flag else y\n";
        let pairs = ternary_mutations(source);
        let swap_muts: Vec<_> = pairs.iter().filter(|(_, m)| m.original.contains("flag")).collect();
        assert_eq!(swap_muts.len(), 1, "x if flag else y must produce exactly one ternary_swap");
        let (_, m) = &swap_muts[0];
        assert_eq!(m.original, "x if flag else y");
        assert_eq!(m.replacement, "y if flag else x", "body and orelse must be swapped; condition stays");
    }

    // INV-1: every generated mutation produces parseable Python
    #[test]
    fn test_parseable() {
        let sources = [
            "def f(ok):\n    return \"yes\" if ok else \"no\"\n",
            "def f(cond, a, b):\n    return f(a) if cond else g(b)\n",
            "def f(c1, c2):\n    return a if c1 else (b if c2 else d)\n",
        ];
        for source in &sources {
            let fms = collect_file_mutations(source);
            for fm in &fms {
                for m in fm.mutations.iter().filter(|m| m.operator == "ternary_swap") {
                    let mutated = apply_mutation(&fm.source, m);
                    assert!(
                        parse_module(&mutated, None).is_ok(),
                        "mutated source must be parseable:\n{mutated}"
                    );
                }
            }
        }
    }

    // Identical branches must NOT generate a ternary_swap (equivalent mutant)
    #[test]
    fn test_identical_branches_skipped() {
        let source = "def f(cond):\n    return x if cond else x\n";
        let pairs = ternary_mutations(source);
        assert!(pairs.is_empty(), "identical branches must not produce ternary_swap mutation");
    }

    // String literals: "yes" if ok else "no"
    #[test]
    fn test_string_branches() {
        let source = "def f(ok):\n    return \"yes\" if ok else \"no\"\n";
        let pairs = ternary_mutations(source);
        let swap_muts: Vec<_> = pairs.iter().filter(|(_, m)| m.original.contains("ok")).collect();
        assert_eq!(swap_muts.len(), 1);
        let (_, m) = &swap_muts[0];
        assert!(m.replacement.starts_with("\"no\""), "orelse must become body: {}", m.replacement);
        assert!(m.replacement.ends_with("\"yes\""), "body must become orelse: {}", m.replacement);
        assert!(m.replacement.contains("ok"), "condition must be preserved: {}", m.replacement);
    }

    // Ternary in a function call: f(a if c else b) — still generates mutation
    #[test]
    fn test_ternary_inside_call() {
        let source = "def f(c, a, b):\n    return g(a if c else b)\n";
        let pairs = ternary_mutations(source);
        assert!(!pairs.is_empty(), "ternary inside a call must still generate ternary_swap");
        let (_, m) = &pairs[0];
        assert_eq!(m.original, "a if c else b");
        assert_eq!(m.replacement, "b if c else a");
    }

    // Nested ternary: each level gets its own swap independently
    #[test]
    fn test_nested_ternary() {
        let source = "def f(c1, c2):\n    return a if c1 else (b if c2 else d)\n";
        let pairs = ternary_mutations(source);
        let swap_originals: Vec<&str> = pairs.iter().map(|(_, m)| m.original.as_str()).collect();
        // Outer swap
        assert!(
            swap_originals.iter().any(|s| s.contains("c1")),
            "outer ternary must be swapped; got: {swap_originals:?}"
        );
        // Inner swap
        assert!(
            swap_originals.iter().any(|s| s.contains("c2") && !s.contains("c1")),
            "inner ternary must be swapped independently; got: {swap_originals:?}"
        );
    }

    // Span correctness: fm.source[m.start..m.end] == m.original
    #[test]
    fn test_span_correctness() {
        let source = "def f(flag):\n    return x if flag else y\n";
        let pairs = ternary_mutations(source);
        for (fm, m) in &pairs {
            let slice = &fm.source[m.start..m.end];
            assert_eq!(
                slice, m.original.as_str(),
                "source slice at [{}..{}] must equal original '{}', got '{}'",
                m.start, m.end, m.original, slice
            );
        }
    }
}
