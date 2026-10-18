#![no_main]

use libfuzzer_sys::fuzz_target;

// The whole native reader (ZIP, chunk tree, references, model), the version probe and the
// preview, on arbitrary bytes.
fuzz_target!(|bytes: &[u8]| {
    let limits = vectorcraft_cdr::Limits { max_entry: 16 << 20, max_total: 64 << 20, ..vectorcraft_cdr::Limits::default() };
    let _ = vectorcraft_cdr::is_cdr(bytes);
    let _ = vectorcraft_cdr::version(bytes);
    let _ = vectorcraft_cdr::preview(bytes);
    let _ = vectorcraft_cdr::read(bytes, limits);
});
