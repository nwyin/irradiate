#![no_main]
use libfuzzer_sys::fuzz_target;
use irradiate::mutation::collect_file_mutations;

fuzz_target!(|data: &[u8]| {
    if let Ok(source) = std::str::from_utf8(data) {
        let _ = collect_file_mutations(source);
    }
});
