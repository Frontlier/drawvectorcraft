//! Reader for native CorelDRAW documents (`.cdr`): the ZIP container, the RIFF chunk tree in
//! `content/root.dat` with its payloads in `content/data/*.dat`, and the subset of the document
//! model VectorCraft imports (pages, layers, groups, curves, rectangles, ellipses, PowerClips,
//! fills, outlines, transparency, artistic text and bitmaps).
//!
//! The layouts were worked out from a user-owned sample file (format version 2600) by observing
//! its bytes and cross-checking them against the file's own metadata and embedded previews. No
//! CorelDRAW software was used and no third-party CorelDRAW parser was read; see the crate README
//! for provenance and the exact supported subset. Older layouts (`content/riffData.cdr` ZIPs and
//! bare RIFF files) are recognised by [`is_cdr`] but [`read`] refuses them with
//! [`Error::Unsupported`], so a caller can fall back to another importer. Nothing here writes
//! CorelDRAW files.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::fmt;

mod bitmap;
mod container;
mod geometry;
mod json;
pub mod model;
mod paint;
pub mod preview;
pub mod riff;
mod structure;
mod style;
#[cfg(any(test, feature = "synth"))]
pub mod synth;
mod text;
pub mod zip;

pub use container::{Limits, version};
pub use model::*;
pub use preview::{MAX_PREVIEW_BYTES, MAX_PREVIEW_DIMENSION, Preview, preview};
pub use structure::read;

/// The format version (`vrsn`) of the files the reader was checked against. Other versions of the
/// same ZIP layout are read too, with a warning.
pub const VERIFIED_VERSION: u32 = 2600;

/// The `mimetype` entry of CorelDRAW's ZIP documents starts with this.
pub const MIMETYPE_PREFIX: &[u8] = b"application/x-vnd.corel.";

/// Why a file could not be read. Messages are short reasons; callers add advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed(&'static str),
    Unsupported(&'static str),
    Limit(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(s) => write!(f, "damaged CorelDRAW file: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported CorelDRAW file: {s}"),
            Self::Limit(s) => write!(f, "CorelDRAW file exceeds a safety limit: {s}"),
        }
    }
}

impl std::error::Error for Error {}

/// Content sniff on the first bytes of a file (a 4 KiB head is enough; empty input is fine).
///
/// True for a ZIP whose first entry is a stored `mimetype` starting with
/// `application/x-vnd.corel.`, or whose first entry is `content/root.dat` or
/// `content/riffData.cdr`, and for a bare `RIFF....CDR?` file. Never true for an arbitrary ZIP.
pub fn is_cdr(bytes: &[u8]) -> bool {
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes.get(8..11) == Some(b"CDR".as_slice()) {
        return true;
    }
    let Some(head) = zip::first_local_entry(bytes) else { return false };
    match head.name {
        b"mimetype" => head.method == 0 && head.data_prefix.starts_with(MIMETYPE_PREFIX),
        b"content/root.dat" | b"content/riffData.cdr" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
