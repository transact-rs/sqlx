#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Test MySQL length-encoded integer parsing via public API
    // The io module is private, so this is a placeholder for now
    let _ = std::str::from_utf8(data);
});
