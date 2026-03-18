#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    // Split on first NUL byte to get (source, module_name); fall back to fixed name.
    let (source, module_name) = match s.find('\x00') {
        Some(idx) => (&s[..idx], &s[idx + 1..]),
        None => (s, "fuzz_module"),
    };
    let _ = irradiate::codegen::mutate_file(source, module_name);
});
