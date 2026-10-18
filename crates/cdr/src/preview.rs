//! The PNG pictures CorelDRAW stores in its ZIP documents: `previews/thumbnail.png` (a small
//! render of everything on the first page, desktop objects included) and `previews/page1.png`
//! (the first page rectangle). They are CorelDRAW's own renders, never the document itself: a
//! caller shows one only when the native content can't be read.

use crate::zip::Zip;
use crate::{Error, Limits, MIMETYPE_PREFIX};

pub const MAX_PREVIEW_BYTES: usize = 16 << 20;
pub const MAX_PREVIEW_DIMENSION: u32 = 4096;

const THUMBNAIL: &str = "previews/thumbnail.png";
const FIRST_PAGE: &str = "previews/page1.png";

/// An embedded preview picture, at its own dimensions (not the page size).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// The complete PNG file, every chunk checksum verified. Pixels are still compressed: decode
    /// them with allocation limits.
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The embedded thumbnail of a CorelDRAW ZIP document, else its first-page picture.
///
/// Bare-RIFF files (before the ZIP container) carry no PNG preview and give
/// [`Error::Unsupported`]. A ZIP that does not look like a CorelDRAW document is refused.
pub fn preview(bytes: &[u8]) -> Result<Preview, Error> {
    if bytes.starts_with(b"RIFF") {
        return Err(Error::Unsupported("no embedded preview in this older layout"));
    }
    let limits = Limits { max_entry: MAX_PREVIEW_BYTES, max_total: 2 * MAX_PREVIEW_BYTES, ..Limits::default() };
    let zip = Zip::open(bytes, limits)?;
    if !looks_like_cdr(&zip) {
        return Err(Error::Malformed("not a CorelDRAW file"));
    }
    let mut last = Error::Unsupported("no embedded preview");
    for name in [THUMBNAIL, FIRST_PAGE] {
        if zip.entry(name).is_none() {
            continue;
        }
        match zip.read_capped(name, MAX_PREVIEW_BYTES, "preview larger than 16 MiB").and_then(check_png) {
            Ok(p) => return Ok(p),
            // A damaged thumbnail still lets the page picture stand in.
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// A CorelDRAW ZIP names its document stream, or carries CorelDRAW's `mimetype`.
fn looks_like_cdr(zip: &Zip<'_>) -> bool {
    if zip.entry("content/root.dat").is_some() || zip.entry("content/riffData.cdr").is_some() {
        return true;
    }
    zip.entry("mimetype").is_some_and(|e| e.method == 0 && e.size <= 256)
        && zip.read_capped("mimetype", 256, "mimetype too long").is_ok_and(|m| m.starts_with(MIMETYPE_PREFIX))
}

fn bytes_at(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8], Error> {
    let end = offset.checked_add(length).ok_or(Error::Malformed("offset overflow"))?;
    bytes.get(offset..end).ok_or(Error::Malformed("truncated preview"))
}

fn be32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let b = bytes_at(bytes, offset, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Require exactly one complete, plain PNG: signature, IHDR first, every chunk CRC, contiguous
/// IDAT, IEND last with nothing after it, and dimensions within [`MAX_PREVIEW_DIMENSION`].
fn check_png(png: Vec<u8>) -> Result<Preview, Error> {
    if bytes_at(&png, 0, 8)? != b"\x89PNG\r\n\x1a\n" || be32(&png, 8)? != 13 || bytes_at(&png, 12, 4)? != b"IHDR" {
        return Err(Error::Malformed("preview is not a PNG"));
    }
    let width = be32(&png, 16)?;
    let height = be32(&png, 20)?;
    if width == 0 || height == 0 || width > MAX_PREVIEW_DIMENSION || height > MAX_PREVIEW_DIMENSION {
        return Err(Error::Limit("preview dimensions must be 1..4096"));
    }
    // Bound work by bytes, not by a chunk count from the file.
    let mut pos = 8usize;
    let (mut palette, mut image_data, mut image_data_ended) = (false, false, false);
    loop {
        let length = usize::try_from(be32(&png, pos)?).map_err(|_| Error::Limit("PNG chunk length"))?;
        let chunk_size = length.checked_add(12).ok_or(Error::Malformed("PNG chunk overflow"))?;
        let chunk = bytes_at(&png, pos, chunk_size)?;
        let checksum_input = bytes_at(chunk, 4, length + 4)?;
        if crc32fast::hash(checksum_input) != be32(chunk, length + 8)? {
            return Err(Error::Malformed("PNG chunk checksum"));
        }
        let kind = bytes_at(chunk, 4, 4)?;
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(Error::Malformed("PNG chunk type"));
        }
        match kind {
            b"IHDR" if pos == 8 => {}
            b"IHDR" => return Err(Error::Malformed("duplicate PNG header")),
            b"PLTE" => {
                if palette || image_data {
                    return Err(Error::Malformed("PNG palette order"));
                }
                palette = true;
            }
            b"IDAT" => {
                if image_data_ended {
                    return Err(Error::Malformed("non-contiguous PNG image data"));
                }
                image_data = true;
            }
            b"IEND" if !image_data => return Err(Error::Malformed("PNG has no image data")),
            b"IEND" => {}
            _ if kind.first().is_some_and(u8::is_ascii_uppercase) => return Err(Error::Unsupported("unknown critical PNG chunk")),
            _ => {}
        }
        if image_data && kind != b"IDAT" {
            image_data_ended = true;
        }
        // CorelDRAW's previews are plain single-frame PNGs (sRGB, gAMA, pHYs, IDAT). Compressed
        // metadata and animation are refused: pixel limits would not bound them.
        if matches!(kind, b"zTXt" | b"iTXt" | b"iCCP") {
            return Err(Error::Unsupported("compressed PNG metadata in the preview"));
        }
        if matches!(kind, b"acTL" | b"fcTL" | b"fdAT") {
            return Err(Error::Unsupported("animated PNG preview"));
        }
        pos = pos.checked_add(chunk_size).ok_or(Error::Malformed("PNG offset overflow"))?;
        if kind == b"IEND" {
            if length != 0 || pos != png.len() {
                return Err(Error::Malformed("PNG end or trailing data"));
            }
            break;
        }
    }
    Ok(Preview { png, width, height })
}
