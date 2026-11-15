#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // ctx reads sources with `fs::read_to_string` and skips any file that is
    // not UTF-8 (`extract/mod.rs`: `Err(_) => continue`), so non-UTF-8 input
    // is out of scope by construction rather than by omission.
    let Ok(src) = std::str::from_utf8(data) else {
        return;
    };
    // Both grammars on the same input, rather than spending an input byte on
    // the flag: `.ts` and `.tsx` are different grammars over overlapping
    // syntax, and a case that parses under one and not the other is exactly
    // the interesting one.
    for tsx in [false, true] {
        if let Ok(facts) = code_context::extract::typescript::extract(src, tsx) {
            code_context_fuzz::check(&facts, src);
        }
    }
});
