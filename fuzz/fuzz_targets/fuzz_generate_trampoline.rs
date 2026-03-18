#![no_main]
use libfuzzer_sys::{arbitrary, arbitrary::Arbitrary, fuzz_target};
use irradiate::mutation::collect_file_mutations;
use irradiate::trampoline::generate_trampoline;

/// Structured fuzzer input for generate_trampoline.
///
/// We derive real mutations from `source` (via collect_file_mutations) so that
/// Mutation::start/end byte ranges are always valid — preventing false panics
/// from out-of-bounds slice indexing in apply_mutation.  The remaining fields
/// (params_source, return_annotation, flags) are fuzzed freely to exercise
/// parse_param_names and wrapper-code generation.
#[derive(Arbitrary, Debug)]
struct FuzzInput {
    source: String,
    params_source: String,
    return_annotation: String,
    is_async: bool,
    is_generator: bool,
    module_name: String,
    // class_name: None → top-level function; Some(s) → method
    class_name: Option<String>,
}

fuzz_target!(|input: FuzzInput| {
    // Collect real FunctionMutations from the fuzzed source so byte ranges are valid.
    let mut fms = collect_file_mutations(&input.source);
    if fms.is_empty() {
        return;
    }

    // Take the first function and override the fuzz-interesting fields.
    let fm = &mut fms[0];
    fm.params_source = input.params_source;
    fm.return_annotation = input.return_annotation;
    fm.is_async = input.is_async;
    fm.is_generator = input.is_generator;
    fm.class_name = input.class_name;

    let module_name = if input.module_name.is_empty() { "fuzz_module" } else { &input.module_name };
    let _ = generate_trampoline(fm, module_name);
});
