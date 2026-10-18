//! Bitmaps.
//!
//! Pixels live in `bmpt/bmp ` records (payloads in `content/data/Bitmaps.dat`), keyed by the u32
//! at the start of each record; a bitmap object (loda objType 5) names one by id.
//!
//! ```text
//! bmp: u32 id | u32 1 | u32 'UI' (0x4955) | u32 size (= length − 8) | u32 0
//!      u32 image block @0x14, u32 mask block @0x18 (both relative to offset 8; 0 = no mask) | …
//! RI block: u16 'RI' (0x4952) | u32 block size | u32 0 | u32 pixel data offset (from the block)
//!      info @+0x0E: u32 model, u32, u32 width, u32 height, u32 planes, u32 bits per pixel,
//!      u32 stride, u32 image size, u32, u32 x and y resolution (pixels per km), u32 palette
//!      offset (from the info, 0 = none), u32 data offset (from the info), u32[3]
//! palette: u16 kind, u16 count, count × B, G, R
//! ```
//!
//! Models verified on a sample: 1 (24-bit B, G, R), 10 (8-bit indexed) and 99 (an 8-bit alpha
//! mask). Rows are stored bottom-up, padded to the stride with garbage. Any other model is
//! reported and the object keeps its outline. The resolution is not read: the bitmap object's
//! frame already places the picture (its aspect matches width/xRes : height/yRes in the sample).

use std::cell::OnceCell;
use std::collections::HashMap;
use std::sync::Arc;

use crate::container::Container;
use crate::geometry::{BitmapRecord, u16_at, u32_at};
use crate::model::{Affine, Image, Path, Warnings};

/// Most bitmap records indexed.
const MAX_BITMAPS: usize = 100_000;
/// Largest side of a picture.
const MAX_SIDE: u32 = 65_535;
/// Most pixels of one picture (256 MiB as RGBA), whatever the shared budget allows.
pub(crate) const MAX_IMAGE_PIXELS: u64 = 1 << 26;

const UI: u32 = 0x4955;
const RI: u16 = 0x4952;
const INFO: usize = 0x0E;
const INFO_SIZE: usize = 64;

const MODEL_BGR: u32 = 1;
const MODEL_INDEXED: u32 = 10;
const MODEL_MASK: u32 = 99;

/// The document's bitmap records, indexed by id on first use.
#[derive(Debug, Default)]
pub(crate) struct Store {
    list: Option<usize>,
    ids: OnceCell<HashMap<u32, usize>>,
    /// Pictures decoded so far, by record: every object that shows a picture shares its pixels,
    /// which are decoded and charged to the budget once.
    pictures: HashMap<usize, (u32, u32, Arc<Vec<u8>>)>,
}

impl Store {
    /// Remember `bmpt` (`root` is the RIFF root list). Records are read only when a bitmap object
    /// needs one.
    pub(crate) fn load(c: &Container<'_>, root: usize, _w: &mut Warnings) -> Self {
        Self { list: c.tree.child_list(root, b"bmpt"), ids: OnceCell::new(), pictures: HashMap::new() }
    }

    fn ids(&self, c: &Container<'_>, w: &mut Warnings) -> &HashMap<u32, usize> {
        self.ids.get_or_init(|| {
            let mut ids = HashMap::new();
            let Some(list) = self.list else { return ids };
            for leaf in c.tree.children(list) {
                if !c.tree.get(leaf).is_some_and(|ch| !ch.list && &ch.id == b"bmp ") {
                    continue;
                }
                if ids.len() >= MAX_BITMAPS {
                    w.warn("too many bitmaps (the rest are ignored)");
                    break;
                }
                match c.payload(leaf).ok().and_then(|b| u32_at(b, 0)) {
                    Some(id) if ids.contains_key(&id) => w.warn("bitmaps with a repeated id (the first is used)"),
                    Some(id) => {
                        ids.insert(id, leaf);
                    }
                    None => w.warn("bitmap records that could not be read (ignored)"),
                }
            }
            ids
        })
    }
}

/// Why a picture was not decoded.
enum Skip {
    Damaged(&'static str),
    Unsupported,
    TooLarge,
    Budget,
}

/// The picture of a bitmap object: `pixel_to_page` maps its pixel space (0..w × 0..h, y down) to
/// page space and `clip` is its crop outline in page space when it is cropped. Decoded pixels are
/// charged to `pixels_left` once per picture (objects showing the same picture share its pixels);
/// `None` (with a warning) leaves the object to its outline.
pub(crate) fn decode(
    store: &mut Store,
    c: &Container<'_>,
    record: &BitmapRecord,
    pixel_to_page: Affine,
    clip: Option<Path>,
    pixels_left: &mut u64,
    w: &mut Warnings,
) -> Option<Image> {
    let Some(&leaf) = store.ids(c, w).get(&record.bitmap_id) else {
        w.warn("bitmaps whose pixels are missing (shown as their outline)");
        return None;
    };
    let (width, height, rgba) = match store.pictures.get(&leaf) {
        Some((width, height, rgba)) => (*width, *height, Arc::clone(rgba)),
        None => {
            let Ok(payload) = c.payload(leaf) else {
                w.warn("bitmaps whose pixels could not be read (shown as their outline)");
                return None;
            };
            let (width, height, rgba) = match picture(payload, pixels_left, w) {
                Ok(p) => p,
                Err(Skip::Damaged(e)) => {
                    w.warn(format!("bitmaps that could not be decoded: {e} (shown as their outline)"));
                    return None;
                }
                Err(Skip::Unsupported) => {
                    w.warn("bitmaps in a pixel format this reader does not know (shown as their outline)");
                    return None;
                }
                Err(Skip::TooLarge) => {
                    w.warn("bitmaps larger than 67 million pixels (shown as their outline)");
                    return None;
                }
                Err(Skip::Budget) => {
                    w.warn("bitmaps beyond the pixel budget (shown as their outline)");
                    return None;
                }
            };
            let rgba = Arc::new(rgba);
            store.pictures.insert(leaf, (width, height, Arc::clone(&rgba)));
            (width, height, rgba)
        }
    };
    let mut transform = pixel_to_page;
    if i64::from(width) != i64::from(record.width) || i64::from(height) != i64::from(record.height) {
        // The object's frame was placed for its own pixel size: stretch the stored picture over it.
        w.warn("bitmap objects whose pixel size disagrees with their picture (the picture is fitted to the frame)");
        let sx = f64::from(record.width) / f64::from(width);
        let sy = f64::from(record.height) / f64::from(height);
        transform = Affine::scale_non_uniform(sx, sy).then(pixel_to_page);
    }
    if !transform.is_finite() {
        return None;
    }
    Some(Image { width, height, rgba, transform, clip })
}

/// One RI block, validated: its model, size, rows and palette.
struct Block<'a> {
    model: u32,
    width: u32,
    height: u32,
    bits: u32,
    stride: usize,
    /// `stride × height` bytes, bottom row first.
    data: &'a [u8],
    palette: Option<&'a [u8]>,
}

fn block(b: &[u8], at: usize) -> Result<Block<'_>, Skip> {
    let bad = Skip::Damaged;
    if u16_at(b, at) != Some(RI) {
        return Err(bad("a missing pixel block"));
    }
    let field = |o: usize| u32_at(b, at + o).ok_or(Skip::Damaged("a truncated pixel block"));
    let size = field(2)? as usize;
    let data_off = field(0x0A)? as usize;
    let blk = b.get(at..at.checked_add(size).ok_or(bad("a pixel block outside its record"))?).ok_or(bad("a pixel block outside its record"))?;
    let info = |o: usize| u32_at(blk, INFO + o).ok_or(Skip::Damaged("a truncated pixel block"));
    let (model, width, height, planes, bits, stride, image_size) = (info(0)?, info(8)?, info(12)?, info(16)?, info(20)?, info(24)?, info(28)?);
    let (palette_off, data_off_info) = (info(44)? as usize, info(48)? as usize);
    if data_off < INFO + INFO_SIZE || data_off_info.checked_add(INFO) != Some(data_off) {
        return Err(bad("pixel data offsets that disagree"));
    }
    if !(1..=MAX_SIDE).contains(&width) || !(1..=MAX_SIDE).contains(&height) || planes != 1 {
        return Err(bad("an impossible picture size"));
    }
    let stride = stride as usize;
    let row = (width as usize).checked_mul(bits as usize).ok_or(bad("an impossible picture size"))?.div_ceil(8);
    let rows = stride.checked_mul(height as usize).filter(|n| *n <= image_size as usize);
    let (Some(rows), true) = (rows, stride >= row) else { return Err(bad("a row stride too small for the picture")) };
    let data =
        blk.get(data_off..data_off.checked_add(rows).ok_or(bad("pixel data outside its block"))?).ok_or(bad("pixel data outside its block"))?;
    let palette = if palette_off == 0 {
        None
    } else {
        // The palette lies between the info header and the pixel data, which is inside the block,
        // so none of these sums can overflow.
        if palette_off >= data_off_info {
            return Err(bad("a damaged palette"));
        }
        let p = INFO + palette_off;
        let count = usize::from(u16_at(blk, p + 2).ok_or(bad("a truncated palette"))?);
        if count > 256 || palette_off + 4 + 3 * count > data_off_info {
            return Err(bad("a damaged palette"));
        }
        Some(blk.get(p + 4..p + 4 + 3 * count).ok_or(bad("a truncated palette"))?)
    };
    Ok(Block { model, width, height, bits, stride, data, palette })
}

/// Decode a `bmp ` record into straight-alpha RGBA, top row first.
fn picture(b: &[u8], pixels_left: &mut u64, w: &mut Warnings) -> Result<(u32, u32, Vec<u8>), Skip> {
    let bad = Skip::Damaged;
    if u32_at(b, 8) != Some(UI) {
        return Err(bad("a bitmap record without its header"));
    }
    if u32_at(b, 12).and_then(|s| (s as usize).checked_add(8)) != Some(b.len()) {
        return Err(bad("a bitmap record of the wrong length"));
    }
    let image_at = u32_at(b, 0x14).and_then(|o| (o as usize).checked_add(8)).ok_or(bad("a bitmap record without its header"))?;
    let mask_at = u32_at(b, 0x18).ok_or(bad("a bitmap record without its header"))? as usize;
    let image = block(b, image_at)?;
    match (image.model, image.bits, image.palette.is_some()) {
        (MODEL_BGR, 24, _) | (MODEL_INDEXED, 8, true) => {}
        _ => return Err(Skip::Unsupported),
    }
    let mask = if mask_at == 0 {
        None
    } else {
        match block(b, mask_at.saturating_add(8)) {
            Ok(m) if m.model == MODEL_MASK && m.bits == 8 && m.width == image.width && m.height == image.height => Some(m),
            _ => {
                w.warn("bitmap transparency masks that could not be used (pictures drawn opaque)");
                None
            }
        }
    };
    let (width, height) = (image.width, image.height);
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_IMAGE_PIXELS {
        return Err(Skip::TooLarge);
    }
    if pixels > *pixels_left {
        return Err(Skip::Budget);
    }
    let bytes = usize::try_from(pixels * 4).map_err(|_| Skip::TooLarge)?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(bytes).map_err(|_| Skip::Budget)?;
    *pixels_left -= pixels;
    let (w_px, h_px) = (width as usize, height as usize);
    for y in 0..h_px {
        // Rows are stored bottom-up.
        let stored = h_px - 1 - y;
        let row = image.data.get(stored * image.stride..).unwrap_or_default();
        let alpha = mask.as_ref().and_then(|m| m.data.get(stored * m.stride..stored * m.stride + w_px));
        for x in 0..w_px {
            let [r, g, b] = match image.model {
                MODEL_BGR => match row.get(3 * x..3 * x + 3) {
                    Some(p) => [p[2], p[1], p[0]],
                    None => [0, 0, 0],
                },
                _ => {
                    let index = row.get(x).map_or(0, |i| usize::from(*i));
                    match image.palette.and_then(|p| p.get(3 * index..3 * index + 3)) {
                        Some(p) => [p[2], p[1], p[0]],
                        None => [0, 0, 0],
                    }
                }
            };
            rgba.extend([r, g, b, alpha.and_then(|a| a.get(x).copied()).unwrap_or(255)]);
        }
    }
    Ok((width, height, rgba))
}

/// A decoded picture for tests: width, height and RGBA.
#[cfg(test)]
pub(crate) fn decode_record(b: &[u8]) -> crate::geometry::Decode<(u32, u32, Vec<u8>)> {
    let mut budget = u64::MAX;
    let mut w = Warnings::default();
    picture(b, &mut budget, &mut w).map_err(|e| match e {
        Skip::Damaged(s) => s,
        Skip::Unsupported => "unsupported",
        Skip::TooLarge => "too large",
        Skip::Budget => "budget",
    })
}
