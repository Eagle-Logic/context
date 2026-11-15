#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // ctx reads sources with `fs::read_to_string` and skips any file that is
    // not UTF-8 (`extract/mod.rs`: `Err(_) => continue`), so non-UTF-8 input
    // is out of scope by construction rather than by omission.
    let Ok(src) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(facts) = code_context::extract::rust::extract(src) {
        code_context_fuzz::check(&facts, src);
    }
});
