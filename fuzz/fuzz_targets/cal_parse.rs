#![no_main]
use libfuzzer_sys::fuzz_target;

// The CAL parser runs on untrusted query strings (console, MCP, API). Malformed
// input must return an error, never panic — and the length/nesting guards must
// hold on adversarial input. `parse` drives the lexer + parser together.
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = areev_cal::parse(s);
    }
});
