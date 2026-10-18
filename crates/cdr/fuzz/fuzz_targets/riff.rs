#![no_main]

use libfuzzer_sys::fuzz_target;
use vectorcraft_cdr::synth::{self, Method};

// The chunk tree and every record decoder. The input replaces, in turn, `content/root.dat`, each
// data file and each side entry (font table, palette) of the synthetic sample inside stored ZIP
// entries written with valid checksums, so mutations reach the record grammar instead of the
// archive's CRC.
fuzz_target!(|bytes: &[u8]| {
    let limits = vectorcraft_cdr::Limits { max_entry: 16 << 20, max_total: 64 << 20, ..vectorcraft_cdr::Limits::default() };
    let _ = vectorcraft_cdr::riff::Tree::parse(bytes);
    let sample = synth::sample_parts();
    let mut parts = sample.clone();
    parts.root = bytes.to_vec();
    let _ = vectorcraft_cdr::read(&parts.zip(Method::Stored), limits);
    for i in 0..sample.data.len() {
        let mut parts = sample.clone();
        if let Some(file) = parts.data.get_mut(i) {
            file.1 = bytes.to_vec();
        }
        let _ = vectorcraft_cdr::read(&parts.zip(Method::Stored), limits);
    }
    for i in 0..sample.extra.len() {
        let mut parts = sample.clone();
        if let Some(entry) = parts.extra.get_mut(i) {
            entry.1 = bytes.to_vec();
        }
        let _ = vectorcraft_cdr::read(&parts.zip(Method::Stored), limits);
    }
});
