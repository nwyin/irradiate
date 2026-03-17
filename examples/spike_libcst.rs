//! Spike: parse Python, mutate a binary operator, codegen back.

use libcst_native::{
    parse_module, Codegen, CodegenState, CompoundStatement, Expression, SmallStatement, Statement,
};

const PYTHON_SOURCE: &str = r#"def add(a, b):
    return a + b
"#;

fn main() {
    println!("=== Original ===\n{PYTHON_SOURCE}");

    let module = parse_module(PYTHON_SOURCE, None).expect("Failed to parse");

    // Roundtrip unchanged
    let mut state = CodegenState::default();
    module.codegen(&mut state);
    println!("=== Roundtrip ===\n{}", state.tokens);
    assert_eq!(state.tokens, PYTHON_SOURCE, "Roundtrip must be lossless");

    // Now mutate: swap + to -
    // Since CST nodes don't derive Clone, we need to reconstruct.
    // But we can parse the function body separately and swap the operator.
    // Let's try a simpler approach: use codegen per-function, then do text-level mutation.

    // Extract the function
    let func = match &module.body[0] {
        Statement::Compound(CompoundStatement::FunctionDef(f)) => f,
        _ => panic!("expected function"),
    };

    // Get the return value expression
    let ret_expr = match &func.body {
        libcst_native::Suite::IndentedBlock(block) => match &block.body[0] {
            Statement::Simple(s) => match &s.body[0] {
                SmallStatement::Return(r) => r.value.as_ref().unwrap(),
                _ => panic!("expected return"),
            },
            _ => panic!("expected simple statement"),
        },
        _ => panic!("expected indented block"),
    };

    // Codegen just the expression
    let mut expr_state = CodegenState::default();
    ret_expr.codegen(&mut expr_state);
    println!("=== Return expression ===\n{}", expr_state.tokens);

    // Verify it's a binary op
    match ret_expr {
        Expression::BinaryOperation(binop) => {
            let mut op_state = CodegenState::default();
            binop.operator.codegen(&mut op_state);
            println!("Operator token: '{}'", op_state.tokens.trim());

            // Codegen left and right
            let mut left_state = CodegenState::default();
            binop.left.codegen(&mut left_state);
            let mut right_state = CodegenState::default();
            binop.right.codegen(&mut right_state);
            println!("Left: '{}', Right: '{}'", left_state.tokens, right_state.tokens);

            // Now generate the mutated expression as text
            let mutated_expr = format!("{} - {}", left_state.tokens, right_state.tokens);
            println!("\n=== Mutated expression ===\n{mutated_expr}");

            // Generate full mutated function
            let mutated_func = format!(
                "def x_add__mutmut_1(a, b):\n    return {mutated_expr}\n"
            );
            println!("\n=== Mutated function ===\n{mutated_func}");

            // Verify it parses
            let _check = parse_module(&mutated_func, None).expect("Mutated code must parse");
            println!("Mutated code parses OK!");
        }
        _ => panic!("expected binary operation"),
    }

    println!("\n=== SPIKE PASSED ===");
    println!("Strategy: parse with libcst for structure, codegen per-node for text,");
    println!("generate mutations as text substitutions, emit trampoline as templates.");
}
