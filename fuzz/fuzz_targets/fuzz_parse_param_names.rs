#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(_s) = std::str::from_utf8(data) else { return };
    // Use the low bit of the first byte to determine has_self; strip it from the input.
    let (has_self, params) = if let Some((&first, rest)) = data.split_first() {
        let Ok(rest_str) = std::str::from_utf8(rest) else { return };
        (first & 1 == 1, rest_str)
    } else {
        (false, "")
    };
    let _ = irradiate::trampoline::parse_param_names(params, has_self);
});
