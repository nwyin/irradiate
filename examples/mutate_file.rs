//! Example: run the mutation engine on a Python source file and print the output.

use irradiate::codegen::mutate_file;

fn main() {
    let source = r#"def add(a, b):
    return a + b

def is_positive(n):
    if n > 0:
        return True
    return False

def greet(name):
    return "Hello, " + name
"#;

    match mutate_file(source, "simple_lib") {
        Some(result) => {
            println!(
                "=== Mutated source ({} mutants) ===\n",
                result.mutant_names.len()
            );
            println!("{}", result.source);
            println!("=== Mutant names ===");
            for name in &result.mutant_names {
                println!("  {name}");
            }
        }
        None => println!("No mutations found"),
    }
}
