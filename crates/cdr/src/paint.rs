//! Fills, outlines, transparency and colours.
//!
//! ```text
//! obj ─ lgob ─ loda ─ arg 20   u32 fill id ─────────────► doc/filt/filc/fild  (id = u32 @0)
//!            │      ├ arg 10   u32 outline id ──────────► doc/otlt/outl       (id = u32 @0)
//!            │      ├ arg 8000 transparency (inline) ─ fill id ─► fild (a uniform colour or a grey ramp)
//!            │      ├ arg 12000 u32 object style ───────► doc/stlt (only when arg 20/10 are missing)
//!            │      ├ arg 14001 u32 1 = nonzero winding
//!            │      ├ arg 19150 mesh fill definition (objType 32)
//!            │      └ arg 201  JSON cache of the same paint (used only for colour models this reader does not know)
//!            ├ ftil  fill tile matrix (identity or a tile-origin offset: no effect on fill types 0–2)
//!            └ ttil  transparency tile matrix (identity in every sample object)
//! ```
//!
//! `fild`: u32 id, u32 version (2400), u32 guidLen, GUID, then `{u32 tag, u32 len, data}` items up
//! to `(0, 0)`; tag 1300 is the fill body: u16 fill type (0 none, 1 uniform, 2 fountain), then
//! (unless 0) inner items: 1300 the type's body, 1700 a fountain's placement. `outl`: u32 id, u32
//! version (11), u32 guidLen, GUID, items: 3 miter angle (micro-degrees), 2 colour block, 1 pen.
//!
//! A fountain is placed on the object's own box: unit space `u` (linear from (−1, 0) to (1, 0),
//! radial on the unit circle) → `local = C + diag(hw, hh)·(Mf·u + 2·(cx, cy))` → the object's
//! transform, where `C`, `hw`, `hh` are the centre and half-sizes of the object's geometry before
//! its transform, `Mf` the placement matrix and `(cx, cy)` its centre offset. Transparency ramps
//! are placed the same way, their grey levels read as opacities (white opaque).
//!
//! A record that fails to decode only costs its own paint (with a warning); it never fails the
//! object or the document.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::container::{Container, attribute, unescape};
use crate::geometry::{Decode, Loda, arg, f64_at, kind, u16_at, u32_at};
use crate::json;
use crate::model::{
    Affine, Blend, Cap, Color, Gradient, GradientKind, Join, OpacityMask, OpacityStop, Outline, POINTS_PER_UNIT, Paint, Rect, Stop, Transparency,
    Warnings,
};
use crate::style::Styles;

/// What resolving one object's paint can use.
#[derive(Debug)]
pub(crate) struct Target<'l, 'a> {
    pub(crate) loda: &'l Loda<'a>,
    /// The `ftil` payload of the object's `lgob` (the fill tile matrix). Only an identity or a
    /// tile-origin offset occurs, and neither changes fill types 0–2, so it is not read yet.
    #[allow(dead_code)]
    pub(crate) fill_tile: Option<&'a [u8]>,
    /// The `ttil` payload (48 bytes: the transparency tile matrix), when present.
    pub(crate) transparency_tile: Option<&'a [u8]>,
    /// Bounds of the object's geometry before its transform (fountain fills are placed on them).
    pub(crate) local_bounds: Option<Rect>,
    /// Object space → world space (the object's own `trfd`).
    pub(crate) world: Affine,
    /// Object space → page space.
    pub(crate) to_page: Affine,
}

/// Most fill or outline records indexed per document.
const MAX_RECORDS: usize = 100_000;
/// Most `{tag, len}` items one record may hold before its terminator.
const MAX_ITEMS: usize = 64;
/// Most stops one fountain may have (the sample has at most 4).
const MAX_STOPS: usize = 256;
/// Most tags one colour block may hold before its terminator.
const MAX_COLOR_TAGS: usize = 32;
/// Most nodes of a mesh fill's colour grid.
const MAX_MESH_NODES: usize = 4096;
/// Largest `color/docPalette.xml` read.
const MAX_PALETTE_XML: usize = 1 << 20;
const MAX_PALETTE_ENTRIES: usize = 65_536;
/// Thickest outline accepted, in world units (10 cm); thicker ones are clamped with a warning.
const MAX_WIDTH: u32 = 1_000_000_000;
/// Placement coefficients beyond this are treated as damaged.
const MAX_COEFFICIENT: f64 = 1e9;

const FILL_VERSION: u32 = 2400;
const OUTLINE_VERSION: u32 = 11;
const TAG_BODY: u32 = 1300;
const TAG_PLACEMENT: u32 = 1700;
/// Pen flags: switched off, drawn, behind the fill, scales with the object.
const PEN_OFF: u32 = 0x01;
const PEN_KNOWN: u32 = 0x01 | 0x02 | 0x10 | 0x20;
const PEN_BEHIND: u32 = 0x10;
const PEN_SCALES: u32 = 0x20;

const BLACK: Color = Color::Rgb { r: 0.0, g: 0.0, b: 0.0 };

/// A colour as stored: a model this reader knows, or the id of one it does not.
#[derive(Debug, Clone, PartialEq)]
enum Stored {
    Known(Color),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
enum Fill {
    None,
    Uniform(Stored),
    Fountain(Fountain),
    /// A fill type this reader does not decode (pattern, texture, PostScript…).
    Other(u16),
}

#[derive(Debug, Clone, PartialEq)]
struct Fountain {
    kind: u16,
    /// Midpoint of the first segment (0..100), used when the placement lacks per-stop midpoints.
    midpoint: u16,
    stops: Vec<FountainStop>,
    placement: Option<Placement>,
}

#[derive(Debug, Clone, PartialEq)]
struct FountainStop {
    /// 0..100.
    position: u16,
    color: Stored,
    alpha: u8,
    /// Midpoint (0..100) of the segment after this stop.
    midpoint: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    cx: f64,
    cy: f64,
    /// `x' = m0·x + m1·y + m2`, `y' = m3·x + m4·y + m5` in unit space.
    m: [f64; 6],
}

#[derive(Debug, Clone, PartialEq)]
struct Pen {
    flags: u32,
    join: u16,
    /// World units.
    width: u32,
    /// Calligraphic nib aspect in percent (100 = round).
    stretch: u16,
    color: Stored,
    /// Miter limit angle in micro-degrees.
    miter: Option<u32>,
    /// The pen's caps/dash/arrowhead area is not all zero.
    extras: bool,
}

/// The document palette (`color/docPalette.xml`): colours a fill names by the palette's GUID.
#[derive(Debug, Clone, PartialEq)]
struct Palette {
    guid: [u8; 16],
    name: Option<String>,
    entries: Vec<PaletteEntry>,
}

#[derive(Debug, Clone, PartialEq)]
struct PaletteEntry {
    cmyk: bool,
    name: String,
    /// Components 0..1.
    tints: Vec<f64>,
}

/// The document's paint records, indexed by id.
#[derive(Debug, Default)]
pub(crate) struct Tables {
    fills: HashMap<u32, Decode<Fill>>,
    outlines: HashMap<u32, Decode<Pen>>,
    /// The style table, shared with the text decoder.
    pub(crate) styles: Option<Styles>,
}

impl Tables {
    /// Index `doc/filt`, `doc/otlt` and `doc/stlt` (`doc` is the `doc ` list).
    pub(crate) fn load(c: &Container<'_>, doc: usize, w: &mut Warnings) -> Self {
        let palette = Palette::load(c);
        let mut fills = HashMap::new();
        if let Some(filt) = c.tree.child_list(doc, b"filt") {
            for item in c.tree.children(filt) {
                let Some(ch) = c.tree.get(item) else { continue };
                let leaf = match (ch.list, &ch.id) {
                    (true, b"filc") => c.tree.child_leaf(item, b"fild"),
                    (false, b"fild") => Some(item),
                    _ => None,
                };
                let Some(leaf) = leaf else { continue };
                if fills.len() >= MAX_RECORDS {
                    w.warn("too many fill records (the rest are ignored)");
                    break;
                }
                let Some((id, fill)) = c.payload(leaf).ok().and_then(|b| Some((u32_at(b, 0)?, fill_record(b, palette.as_ref(), w)))) else {
                    w.warn("fill records that could not be read (ignored)");
                    continue;
                };
                if let Entry::Vacant(slot) = fills.entry(id) {
                    slot.insert(fill);
                } else {
                    w.warn("fill records with a repeated id (the first is used)");
                }
            }
        }
        let mut outlines = HashMap::new();
        if let Some(otlt) = c.tree.child_list(doc, b"otlt") {
            for leaf in c.tree.children(otlt) {
                if !c.tree.get(leaf).is_some_and(|ch| !ch.list && &ch.id == b"outl") {
                    continue;
                }
                if outlines.len() >= MAX_RECORDS {
                    w.warn("too many outline records (the rest are ignored)");
                    break;
                }
                let Some((id, pen)) = c.payload(leaf).ok().and_then(|b| Some((u32_at(b, 0)?, pen_record(b, palette.as_ref(), w)))) else {
                    w.warn("outline records that could not be read (ignored)");
                    continue;
                };
                if let Entry::Vacant(slot) = outlines.entry(id) {
                    slot.insert(pen);
                } else {
                    w.warn("outline records with a repeated id (the first is used)");
                }
            }
        }
        Self { fills, outlines, styles: Styles::load(c, doc, w) }
    }

    /// The id a 4-byte paint argument names, else the one the object's style names.
    fn paint_id(&self, loda: &Loda<'_>, id: u32, from_style: fn(&Styles, u32) -> Option<u32>, w: &mut Warnings) -> Option<u32> {
        match loda.arg(id) {
            Some(d) if d.len() == 4 => return u32_at(d, 0),
            Some(_) => {
                w.warn("paint references of an unexpected size (ignored)");
                return None;
            }
            None => {}
        }
        let style = loda.u32_arg(arg::STYLE).filter(|s| *s != 0)?;
        let found = self.styles.as_ref().and_then(|s| from_style(s, style));
        if found.is_some() {
            w.warn("paint taken from an object style (unverified)");
        }
        found
    }

    /// The fill of a shape or text (loda arg 20 → `fild`), in page space.
    pub(crate) fn fill(&self, t: &Target<'_, '_>, w: &mut Warnings) -> Paint {
        if t.loda.obj_type == kind::MESH && t.loda.arg(arg::FILL).is_none() {
            return mesh(t.loda, w);
        }
        match self.paint_id(t.loda, arg::FILL, Styles::fill, w) {
            Some(id) => self.fill_by_id(id, t, w),
            None => Paint::None,
        }
    }

    /// The fill record `id` placed on the target.
    pub(crate) fn fill_by_id(&self, id: u32, t: &Target<'_, '_>, w: &mut Warnings) -> Paint {
        let cache = StyleCache::new(t.loda);
        match self.fills.get(&id) {
            None => {
                w.warn("objects whose fill record is missing (not filled)");
                Paint::None
            }
            Some(Err(e)) => {
                w.warn(format!("fills that could not be decoded: {e} (not filled)"));
                Paint::None
            }
            Some(Ok(Fill::None)) => Paint::None,
            Some(Ok(Fill::Uniform(c))) => Paint::Solid(resolve(c, Slot::Uniform(&["fill"]), &cache, w)),
            Some(Ok(Fill::Fountain(f))) => {
                let stops: Vec<Stop> = ramp(f, &["fill"], &cache, w)
                    .into_iter()
                    .map(|(offset, color, alpha, midpoint)| Stop { offset, color, opacity: alpha, midpoint })
                    .collect();
                match stops.len() {
                    0 => {
                        w.warn("fountain fills without colours (not filled)");
                        Paint::None
                    }
                    1 => {
                        w.warn("fountain fills with a single colour (filled with it)");
                        stops.into_iter().next().map_or(Paint::None, |s| Paint::Solid(s.color))
                    }
                    _ => {
                        let kind = gradient_kind(f.kind, w);
                        Paint::Gradient(Gradient { kind, stops, transform: placement(f.placement.as_ref(), kind, t, w) })
                    }
                }
            }
            Some(Ok(Fill::Other(ty))) => {
                w.warn(format!("pattern, texture, PostScript and other fills (fill type {ty}; not imported)"));
                Paint::Unsupported("a pattern, texture or PostScript fill")
            }
        }
    }

    /// The outline of a shape or text (loda arg 10 → `outl`), in page space. `None` when the pen
    /// is switched off or has no width.
    pub(crate) fn outline(&self, t: &Target<'_, '_>, w: &mut Warnings) -> Option<Outline> {
        let id = self.paint_id(t.loda, arg::OUTLINE, Styles::outline, w)?;
        self.outline_by_id(id, t, w)
    }

    /// The outline record `id` for the target.
    pub(crate) fn outline_by_id(&self, id: u32, t: &Target<'_, '_>, w: &mut Warnings) -> Option<Outline> {
        let pen = match self.outlines.get(&id) {
            None => {
                w.warn("objects whose outline record is missing (no outline)");
                return None;
            }
            Some(Err(e)) => {
                w.warn(format!("outlines that could not be decoded: {e} (no outline)"));
                return None;
            }
            Some(Ok(p)) => p,
        };
        if pen.flags & PEN_OFF != 0 || pen.width == 0 {
            return None;
        }
        if pen.flags & !PEN_KNOWN != 0 {
            w.warn("outline settings this reader does not know (ignored)");
        }
        if pen.extras {
            w.warn("outline dashes, arrowheads or line caps (not decoded; drawn solid with butt caps)");
        }
        if pen.stretch != 100 && pen.stretch != 0 {
            w.warn("calligraphic outlines (drawn with a round pen)");
        }
        let join = match pen.join {
            0 => Join::Miter,
            1 => Join::Round,
            2 => Join::Bevel,
            _ => {
                w.warn("outline corners of an unknown kind (mitred)");
                Join::Miter
            }
        };
        let mut width = pen.width;
        if width > MAX_WIDTH {
            w.warn("outlines thicker than 10 cm (clamped)");
            width = MAX_WIDTH;
        }
        let scales = pen.flags & PEN_SCALES != 0;
        // World units → points; with "scale with object" the stored width is in object space.
        let factor = if scales { t.to_page.max_scale() } else { POINTS_PER_UNIT };
        let width = f64::from(width) * factor;
        if !width.is_finite() {
            return None;
        }
        let cache = StyleCache::new(t.loda);
        Some(Outline {
            width,
            paint: Paint::Solid(resolve(&pen.color, Slot::Plain(&["outline", "color"]), &cache, w)),
            cap: Cap::Butt,
            join,
            miter_limit: miter_ratio(pen.miter),
            dash: None,
            behind_fill: pen.flags & PEN_BEHIND != 0,
            scale_with_object: scales,
        })
    }

    /// The transparency (loda arg 8000) and merge mode of an object.
    pub(crate) fn transparency(&self, t: &Target<'_, '_>, w: &mut Warnings) -> (Transparency, Blend) {
        let Some(record) = t.loda.arg(arg::TRANSPARENCY) else { return (Transparency::None, Blend::Normal) };
        let Some(tr) = transparency_record(record) else {
            w.warn("transparency that could not be decoded (ignored)");
            return (Transparency::None, Blend::Normal);
        };
        let blend = match tr.mode {
            0 => Blend::Normal,
            30 => {
                w.warn("transparency merge mode 30 (drawn as overlay, approximated)");
                Blend::Overlay
            }
            m => {
                w.warn(format!("transparency merge mode {m} (not known; drawn as normal)"));
                Blend::Normal
            }
        };
        if tr.applies_to != 2 {
            w.warn("transparency limited to the fill or the outline (applied to the whole object)");
        }
        if t.transparency_tile.is_some_and(|m| !is_identity(m)) {
            w.warn("transparency tile offsets (ignored)");
        }
        let transparency = match self.fills.get(&tr.fill) {
            None | Some(Err(_)) => {
                w.warn("transparency whose ramp record is missing (ignored)");
                Transparency::None
            }
            Some(Ok(Fill::None)) => Transparency::None,
            // The amount is the start value; the colour of the referenced fill is not.
            Some(Ok(Fill::Uniform(_))) => Transparency::Uniform((1.0 - tr.start / 100.0).clamp(0.0, 1.0)),
            Some(Ok(Fill::Fountain(f))) => {
                let cache = StyleCache::new(t.loda);
                let stops: Vec<OpacityStop> = ramp(f, &["transparency", "fill"], &cache, w)
                    .into_iter()
                    .map(|(offset, color, _, midpoint)| OpacityStop { offset, opacity: lightness(&color), midpoint })
                    .collect();
                if stops.is_empty() {
                    w.warn("transparency ramps without stops (ignored)");
                    Transparency::None
                } else {
                    let kind = gradient_kind(f.kind, w);
                    Transparency::Gradient(OpacityMask { kind, stops, transform: placement(f.placement.as_ref(), kind, t, w) })
                }
            }
            Some(Ok(Fill::Other(_))) => {
                w.warn("pattern and texture transparency (not imported)");
                Transparency::Unsupported("a pattern or texture transparency")
            }
        };
        (transparency, blend)
    }
}

/// Curves fill nonzero when loda arg 14001 is 1, even-odd otherwise (INFERRED by the paint
/// prototype; every multi-subpath curve in the sample carries the argument).
pub(crate) fn even_odd(loda: &Loda<'_>) -> bool {
    loda.u32_arg(arg::WINDING) != Some(1)
}

// ---------------------------------------------------------------------------------------------
// Records

/// `{u32 tag, u32 len, data}` items from `at` until `(0, 0)` or the end of `b`.
fn items(b: &[u8], mut at: usize) -> Decode<Vec<(u32, &[u8])>> {
    const BAD: &str = "a damaged paint record";
    let mut out = Vec::new();
    for _ in 0..MAX_ITEMS {
        if at >= b.len() {
            return Ok(out);
        }
        let tag = u32_at(b, at).ok_or(BAD)?;
        let len = u32_at(b, at + 4).ok_or(BAD)? as usize;
        if tag == 0 && len == 0 {
            return Ok(out);
        }
        let start = at + 8;
        let end = start.checked_add(len).ok_or(BAD)?;
        out.push((tag, b.get(start..end).ok_or(BAD)?));
        at = end;
    }
    Err("a paint record with too many parts")
}

fn item<'b>(items: &[(u32, &'b [u8])], tag: u32) -> Option<&'b [u8]> {
    items.iter().find(|(t, _)| *t == tag).map(|(_, d)| *d)
}

fn fill_record(b: &[u8], palette: Option<&Palette>, w: &mut Warnings) -> Decode<Fill> {
    const BAD: &str = "a damaged fill record";
    if u32_at(b, 4).ok_or(BAD)? != FILL_VERSION {
        w.warn("fill records of an unfamiliar version (read as the known one)");
    }
    let guid = u32_at(b, 8).ok_or(BAD)? as usize;
    if guid > 64 {
        return Err(BAD);
    }
    let parts = items(b, 12 + guid)?;
    let body = item(&parts, TAG_BODY).ok_or("a fill record without its body")?;
    let fill_type = u16_at(body, 0).ok_or(BAD)?;
    if fill_type == 0 {
        return Ok(Fill::None);
    }
    let inner = items(body, 2)?;
    match fill_type {
        1 => {
            let (color, palette_id, _) = color_block_at(item(&inner, TAG_BODY).ok_or("a uniform fill without its colour")?, 0)?;
            Ok(Fill::Uniform(named(color, palette_id, palette)))
        }
        2 => fountain(item(&inner, TAG_BODY).ok_or("a fountain fill without its colours")?, item(&inner, TAG_PLACEMENT), palette, w)
            .map(Fill::Fountain),
        t => Ok(Fill::Other(t)),
    }
}

/// Fountain body: u16 kind, 16-byte screen, u16 edge pad, u32 angle, u16, 8 bytes, u16 stop mode,
/// u16, u16 first midpoint @38, u16 stop count @40, then stops {colour block, u16 position}.
/// Placement: f64 cx, cy, f64[6] matrix, u32 anisotropic, 12 bytes, then per stop {u8 alpha, u16,
/// u16 midpoint of the next segment}. The angle and the legacy edge pad are not read: the
/// placement matrix supersedes both (the angle equals the matrix's rotation in every sample
/// record), and the halftone screen is a print setting.
fn fountain(body: &[u8], place: Option<&[u8]>, palette: Option<&Palette>, w: &mut Warnings) -> Decode<Fountain> {
    const BAD: &str = "a damaged fountain fill";
    let kind = u16_at(body, 0).ok_or(BAD)?;
    let midpoint = u16_at(body, 38).ok_or(BAD)?;
    let n = usize::from(u16_at(body, 40).ok_or(BAD)?);
    if n > MAX_STOPS {
        return Err("a fountain fill with too many colours");
    }
    let mut at = 42;
    let mut stops = Vec::with_capacity(n);
    for _ in 0..n {
        let (color, palette_id, end) = color_block_at(body, at)?;
        let position = u16_at(body, end).ok_or(BAD)?;
        at = end + 2;
        stops.push(FountainStop { position, color: named(color, palette_id, palette), alpha: 255, midpoint: None });
    }
    let placement = match place {
        None => {
            w.warn("fountain fills without a placement (centred on the object)");
            None
        }
        Some(p) => {
            let found = placement_record(p, &mut stops);
            if found.is_none() {
                w.warn("fountain fill placements that could not be used (centred on the object)");
            }
            found
        }
    };
    Ok(Fountain { kind, midpoint, stops, placement })
}

fn placement_record(p: &[u8], stops: &mut [FountainStop]) -> Option<Placement> {
    let mut at = 80;
    for s in stops.iter_mut() {
        let (Some(alpha), Some(midpoint)) = (p.get(at).copied(), u16_at(p, at + 3)) else { break };
        s.alpha = alpha;
        s.midpoint = Some(midpoint);
        at += 5;
    }
    let cx = f64_at(p, 0)?;
    let cy = f64_at(p, 8)?;
    let mut m = [0.0; 6];
    for (k, v) in m.iter_mut().enumerate() {
        *v = f64_at(p, 16 + 8 * k)?;
    }
    let sane = [cx, cy].iter().chain(&m).all(|v| v.is_finite() && v.abs() <= MAX_COEFFICIENT);
    let det = m[0] * m[4] - m[1] * m[3];
    (sane && det != 0.0 && det.is_finite()).then_some(Placement { cx, cy, m })
}

/// Pen: u32 flags, u16 join, u32 width, u16 stretch, i32 nib angle, f64[6] nib matrix, 12-byte
/// colour copy @64, 16-byte screen @76, then 38 bytes (caps, dashes and arrowheads; not decoded).
fn pen_record(b: &[u8], palette: Option<&Palette>, w: &mut Warnings) -> Decode<Pen> {
    const BAD: &str = "a damaged outline record";
    if u32_at(b, 4).ok_or(BAD)? != OUTLINE_VERSION {
        w.warn("outline records of an unfamiliar version (read as the known one)");
    }
    let guid = u32_at(b, 8).ok_or(BAD)? as usize;
    if guid > 64 {
        return Err(BAD);
    }
    let parts = items(b, 12 + guid)?;
    let pen = item(&parts, 1).ok_or("an outline record without its pen")?;
    if pen.len() < 92 {
        return Err(BAD);
    }
    let color = match item(&parts, 2) {
        Some(block) => {
            let (c, palette_id, _) = color_block_at(block, 0)?;
            named(c, palette_id, palette)
        }
        None => pen.get(64..76).map_or(Stored::Known(BLACK), stored_value),
    };
    Ok(Pen {
        flags: u32_at(pen, 0).ok_or(BAD)?,
        join: u16_at(pen, 4).ok_or(BAD)?,
        width: u32_at(pen, 6).ok_or(BAD)?,
        stretch: u16_at(pen, 10).ok_or(BAD)?,
        color,
        miter: item(&parts, 3).and_then(|m| u32_at(m, 0)),
        extras: pen.get(92..).is_some_and(|r| r.iter().any(|v| *v != 0)),
    })
}

/// Miter limit as a ratio: CorelDRAW stores the angle below which corners are bevelled.
fn miter_ratio(micro_degrees: Option<u32>) -> f64 {
    let Some(angle) = micro_degrees.map(|m| f64::from(m) / 1e6) else { return 10.0 };
    if angle <= 0.0 || angle >= 180.0 {
        return 10.0;
    }
    let r = 1.0 / (angle.to_radians() / 2.0).sin();
    if r.is_finite() { r } else { 10.0 }
}

#[derive(Debug, Clone, Copy)]
struct TransparencyRecord {
    start: f64,
    mode: u32,
    applies_to: u32,
    fill: u32,
}

/// Arg 8000: u32 tag 1300, u32 length, body: … f64 start @61, f64 end @69 (0..100), u32 merge
/// mode @77, u32 applies-to @81, u32 fill id @105.
fn transparency_record(b: &[u8]) -> Option<TransparencyRecord> {
    if u32_at(b, 0)? != TAG_BODY {
        return None;
    }
    let len = u32_at(b, 4)? as usize;
    let body = b.get(8..8usize.checked_add(len)?)?;
    let start = f64_at(body, 61)?;
    let fill = u32_at(body, 105)?;
    Some(TransparencyRecord {
        start: if start.is_finite() { start.clamp(0.0, 100.0) } else { 0.0 },
        mode: u32_at(body, 77)?,
        applies_to: u32_at(body, 81)?,
        fill,
    })
}

fn is_identity(m: &[u8]) -> bool {
    let id = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    m.len() == 48 && id.iter().enumerate().all(|(k, v)| f64_at(m, 8 * k).is_some_and(|x| (x - v).abs() < 1e-9))
}

// ---------------------------------------------------------------------------------------------
// Colours

/// A tagged colour block: `{u8 tag, u32 len, data}` repeated until tag 0 with length 0.
/// Tag 1 (12 bytes): u16 model, u16 palette type, u32 palette index, u8 values[4]. Tag 7 (16
/// bytes): the GUID of the palette the colour was picked from. Models verified on the sample: 2
/// (CMYK 0..100), 3 and 17 (CMYK 0..255), 5 (RGB stored B, G, R) and 9 (grey 0..255).
pub(crate) fn color_block(b: &[u8]) -> Decode<Color> {
    match color_block_at(b, 0)?.0 {
        Stored::Known(c) => Ok(c),
        Stored::Unknown => Err("a colour model this reader does not know"),
    }
}

/// A colour block at `at`: the colour, its palette GUID and the offset after the terminator.
fn color_block_at(b: &[u8], mut at: usize) -> Decode<(Stored, Option<[u8; 16]>, usize)> {
    const BAD: &str = "a damaged colour";
    let mut value = None;
    let mut palette = None;
    for _ in 0..MAX_COLOR_TAGS {
        let tag = *b.get(at).ok_or(BAD)?;
        let len = u32_at(b, at + 1).ok_or(BAD)? as usize;
        at += 5;
        if tag == 0 && len == 0 {
            return value.map(|v| (v, palette, at)).ok_or("a colour without a value");
        }
        let end = at.checked_add(len).ok_or(BAD)?;
        let data = b.get(at..end).ok_or(BAD)?;
        at = end;
        match tag {
            1 if data.len() >= 12 => value = Some(stored_value(data)),
            1 => return Err(BAD),
            7 => palette = <[u8; 16]>::try_from(data).ok(),
            _ => {}
        }
    }
    Err(BAD)
}

/// A 12-byte colour value (`data` holds at least 12 bytes).
fn stored_value(data: &[u8]) -> Stored {
    let (Some(model), Some(v)) = (u16_at(data, 0), data.get(8..12)) else { return Stored::Unknown };
    match color_value(model, [v[0], v[1], v[2], v[3]]) {
        Ok(c) => Stored::Known(c),
        Err(_) => Stored::Unknown,
    }
}

/// One colour value of a verified model, components scaled to 0..1.
pub(crate) fn color_value(model: u16, v: [u8; 4]) -> Decode<Color> {
    let unit = |x: u8, range: f64| (f64::from(x) / range).clamp(0.0, 1.0);
    Ok(match model {
        2 => Color::Cmyk { c: unit(v[0], 100.0), m: unit(v[1], 100.0), y: unit(v[2], 100.0), k: unit(v[3], 100.0) },
        3 | 17 => Color::Cmyk { c: unit(v[0], 255.0), m: unit(v[1], 255.0), y: unit(v[2], 255.0), k: unit(v[3], 255.0) },
        5 => Color::Rgb { r: unit(v[2], 255.0), g: unit(v[1], 255.0), b: unit(v[0], 255.0) },
        9 => Color::Gray { v: unit(v[0], 255.0) },
        _ => return Err("a colour model this reader does not know"),
    })
}

/// A colour string of the style JSON: `"<MODEL>,<PALETTE>,v1,…,vn,<tint 0..100>,<palette GUID>"`
/// with the models seen in the sample (`RGB255`, `CMYK`, `CMYK255`, `CMYK100`, `GRAY255`).
pub(crate) fn color_string(s: &str) -> Option<Color> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    let (model, values) = (parts.first()?, parts.get(2..parts.len().checked_sub(2)?)?);
    let v: Vec<f64> = values.iter().map(|p| p.parse::<f64>().ok().filter(|x| x.is_finite())).collect::<Option<_>>()?;
    let unit = |x: f64, range: f64| (x / range).clamp(0.0, 1.0);
    match (*model, v.as_slice()) {
        ("RGB255", [r, g, b]) => Some(Color::Rgb { r: unit(*r, 255.0), g: unit(*g, 255.0), b: unit(*b, 255.0) }),
        ("CMYK", [c, m, y, k]) => Some(Color::Cmyk { c: unit(*c, 100.0), m: unit(*m, 100.0), y: unit(*y, 100.0), k: unit(*k, 100.0) }),
        ("CMYK255" | "CMYK100", [c, m, y, k]) => Some(Color::Cmyk { c: unit(*c, 255.0), m: unit(*m, 255.0), y: unit(*y, 255.0), k: unit(*k, 255.0) }),
        ("GRAY255", [g]) => Some(Color::Gray { v: unit(*g, 255.0) }),
        _ => None,
    }
}

/// A colour's lightness 0..1 (transparency ramps are grey levels, never colour managed).
fn lightness(c: &Color) -> f64 {
    let v = match c {
        Color::Gray { v } => *v,
        Color::Rgb { r, g, b } => 0.299 * r + 0.587 * g + 0.114 * b,
        Color::Cmyk { c, m, y, k } => 0.299 * (1.0 - c) * (1.0 - k) + 0.587 * (1.0 - m) * (1.0 - k) + 0.114 * (1.0 - y) * (1.0 - k),
        Color::Lab { l, .. } => l / 100.0,
        Color::Named { base, .. } => lightness(base),
    };
    if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }
}

/// Name a colour after the document palette entry of the same value, when the colour carries the
/// palette's id (INFERRED rule; it held for every such colour in the sample).
fn named(color: Stored, palette_id: Option<[u8; 16]>, palette: Option<&Palette>) -> Stored {
    let (Stored::Known(c), Some(id), Some(p)) = (&color, palette_id, palette) else { return color };
    // An all-zero id means "no palette".
    if id != p.guid || id == [0; 16] {
        return color;
    }
    let (cmyk, values) = match c {
        Color::Cmyk { c, m, y, k } => (true, vec![*c, *m, *y, *k]),
        Color::Rgb { r, g, b } => (false, vec![*r, *g, *b]),
        _ => return color,
    };
    let entry = p
        .entries
        .iter()
        .find(|e| e.cmyk == cmyk && e.tints.len() == values.len() && e.tints.iter().zip(&values).all(|(a, b)| (a - b).abs() < 0.6 / 255.0));
    match entry {
        Some(e) => Stored::Known(Color::Named { name: e.name.clone(), palette: p.name.clone(), spot: false, tint: 1.0, base: Box::new(c.clone()) }),
        None => color,
    }
}

impl Palette {
    /// `color/docPalette.xml`: `<palette guid name><colors><page><color cs name tints/>…`, tints
    /// being the components 0..1 for both RGB and CMYK. Optional: any problem means no names.
    fn load(c: &Container<'_>) -> Option<Self> {
        let xml = c.side_entry("color/docPalette.xml", MAX_PALETTE_XML)?;
        let xml = String::from_utf8_lossy(&xml);
        let tag = |from: &str, name: &str| -> Option<(String, usize)> {
            let at = from.find(name)?;
            let rest = from.get(at + name.len()..)?;
            let end = rest.find('>')?;
            Some((rest.get(..end)?.to_string(), at + name.len() + end))
        };
        let (head, mut pos) = tag(&xml, "<palette ")?;
        let guid = guid_bytes(attribute(&head, "guid")?)?;
        let name = attribute(&head, "name").map(unescape).filter(|n| !n.trim().is_empty());
        let mut entries = Vec::new();
        while let Some((attrs, end)) = xml.get(pos..).and_then(|rest| tag(rest, "<color ")) {
            pos += end;
            if entries.len() >= MAX_PALETTE_ENTRIES {
                break;
            }
            let (Some(cs), Some(name), Some(tints)) = (attribute(&attrs, "cs"), attribute(&attrs, "name"), attribute(&attrs, "tints")) else {
                continue;
            };
            let tints: Option<Vec<f64>> = tints.split(',').map(|t| t.trim().parse::<f64>().ok().filter(|v| v.is_finite())).collect();
            let (Some(tints), cmyk) = (tints, cs.eq_ignore_ascii_case("CMYK")) else { continue };
            if cmyk || cs.eq_ignore_ascii_case("RGB") {
                entries.push(PaletteEntry { cmyk, name: unescape(name).chars().take(256).collect(), tints });
            }
        }
        Some(Self { guid, name, entries })
    }
}

/// A GUID string `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` in the byte order the colour blocks
/// store it (the first three groups little-endian).
fn guid_bytes(s: &str) -> Option<[u8; 16]> {
    let s = s.trim().trim_start_matches('{').trim_end_matches('}');
    let groups: Vec<&str> = s.split('-').collect();
    let [a, b, c, d, e] = groups.as_slice() else { return None };
    if a.len() != 8 || b.len() != 4 || c.len() != 4 || d.len() != 4 || e.len() != 12 {
        return None;
    }
    let mut out = Vec::with_capacity(16);
    out.extend(u32::from_str_radix(a, 16).ok()?.to_le_bytes());
    out.extend(u16::from_str_radix(b, 16).ok()?.to_le_bytes());
    out.extend(u16::from_str_radix(c, 16).ok()?.to_le_bytes());
    let rest = format!("{d}{e}");
    for k in 0..8 {
        out.push(u8::from_str_radix(rest.get(2 * k..2 * k + 2)?, 16).ok()?);
    }
    <[u8; 16]>::try_from(out).ok()
}

/// The object's JSON style cache (loda arg 201), parsed only when a colour needs it.
struct StyleCache<'a> {
    raw: Option<&'a [u8]>,
    parsed: OnceCell<Option<json::Value>>,
}

impl<'a> StyleCache<'a> {
    fn new(loda: &Loda<'a>) -> Self {
        Self { raw: loda.arg(arg::STYLE_CACHE), parsed: OnceCell::new() }
    }

    fn get(&self) -> Option<&json::Value> {
        self.parsed
            .get_or_init(|| {
                let raw = self.raw?;
                let n = u32_at(raw, 4)? as usize;
                json::parse(raw.get(8..8usize.checked_add(n)?)?).ok()
            })
            .as_ref()
    }
}

/// Where the style cache keeps a colour: one key, or a fountain stop under a fill object.
#[derive(Debug, Clone, Copy)]
enum Slot<'s> {
    Plain(&'s [&'s str]),
    Uniform(&'s [&'s str]),
    Stop { fill: &'s [&'s str], index: usize, count: usize },
}

impl Slot<'_> {
    fn lookup(self, cache: &StyleCache<'_>) -> Option<Color> {
        let root = cache.get()?;
        let text = match self {
            Slot::Plain(path) => root.path(path)?.as_str()?.to_string(),
            Slot::Uniform(fill) => root.path(fill)?.get("primaryColor")?.as_str()?.to_string(),
            Slot::Stop { fill, index, count } => {
                let f = root.path(fill)?;
                if index == 0 {
                    f.get("primaryColor")?.as_str()?.to_string()
                } else if index + 1 == count {
                    f.get("secondaryColor")?.as_str()?.to_string()
                } else {
                    // "position:colour:alpha:0:midpoint"
                    let s = f.get("intermediateColors")?.as_array()?.get(index - 1)?.as_str()?;
                    s.split(':').nth(1)?.to_string()
                }
            }
        };
        color_string(&text)
    }
}

/// A stored colour, with colours of unknown models looked up in the style cache (else black).
fn resolve(stored: &Stored, slot: Slot<'_>, cache: &StyleCache<'_>, w: &mut Warnings) -> Color {
    match stored {
        Stored::Known(c) => c.clone(),
        Stored::Unknown => match slot.lookup(cache) {
            Some(c) => {
                w.warn("colours in a colour model this reader does not know (taken from the object's style cache)");
                c
            }
            None => {
                w.warn("colours in a colour model this reader does not know (drawn black)");
                BLACK
            }
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Fountains

/// Stops as (offset, colour, alpha, midpoint), offsets clamped and never decreasing.
fn ramp(f: &Fountain, fill: &[&str], cache: &StyleCache<'_>, w: &mut Warnings) -> Vec<(f64, Color, f64, f64)> {
    let count = f.stops.len();
    let mut last = 0.0f64;
    let mut out = Vec::with_capacity(count);
    for (index, s) in f.stops.iter().enumerate() {
        let mut offset = (f64::from(s.position) / 100.0).clamp(0.0, 1.0);
        if offset < last {
            w.warn("fountain colours out of order (reordered)");
            offset = last;
        }
        last = offset;
        let midpoint = s.midpoint.unwrap_or(if index == 0 { f.midpoint } else { 50 });
        let color = resolve(&s.color, Slot::Stop { fill, index, count }, cache, w);
        out.push((offset, color, f64::from(s.alpha) / 255.0, (f64::from(midpoint) / 100.0).clamp(0.0, 1.0)));
    }
    out
}

fn gradient_kind(kind: u16, w: &mut Warnings) -> GradientKind {
    match kind {
        1 => GradientKind::Linear,
        2 => GradientKind::Radial,
        3 => {
            w.warn("conical fountain fills (unverified)");
            GradientKind::Conical
        }
        4 => {
            w.warn("square fountain fills (unverified)");
            GradientKind::Square
        }
        _ => {
            w.warn("fountain fills of an unknown type (drawn radial)");
            GradientKind::Radial
        }
    }
}

/// Gradient space (see [`Gradient::transform`]) → page space.
fn placement(p: Option<&Placement>, kind: GradientKind, t: &Target<'_, '_>, w: &mut Warnings) -> Affine {
    let (cx, cy, m) = p.map_or((0.0, 0.0, [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]), |p| (p.cx, p.cy, p.m));
    let unit = Affine::from_cdr([m[0], m[1], m[2] + 2.0 * cx, m[3], m[4], m[5] + 2.0 * cy]);
    let b = t.local_bounds.unwrap_or_else(|| {
        w.warn("fountains on objects without bounds (placed at the object's origin)");
        Rect { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 }
    });
    let half = |v: f64| if v > 0.0 && v.is_finite() { v / 2.0 } else { 1.0 };
    let frame = Affine([half(b.width()), 0.0, 0.0, half(b.height()), (b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0]);
    // Linear: CorelDRAW's ramp runs from u = (−1, 0) to (1, 0).
    let start = if kind == GradientKind::Linear { Affine([2.0, 0.0, 0.0, 2.0, -1.0, 0.0]) } else { Affine::IDENTITY };
    start.then(unit).then(frame).then(t.to_page)
}

/// Objects of type 32 carry a mesh fill in arg 19150. Only its node colour grid is decoded (`u32
/// 150, u32 rows, u32 cols`, then rows·cols × `{u32 43, colour block}` somewhere in the blob): the
/// shape is filled with the average node colour.
fn mesh(loda: &Loda<'_>, w: &mut Warnings) -> Paint {
    let colors = loda.arg(arg::MESH).and_then(mesh_colors);
    match colors.as_deref().and_then(average) {
        Some(c) => {
            w.warn("mesh fills (filled with their average colour)");
            Paint::Solid(c)
        }
        None => {
            w.warn("mesh fills (not decoded; not filled)");
            Paint::None
        }
    }
}

fn mesh_colors(blob: &[u8]) -> Option<Vec<Color>> {
    'scan: for at in 0..blob.len().saturating_sub(11) {
        if u32_at(blob, at) != Some(150) {
            continue;
        }
        let (Some(rows), Some(cols)) = (u32_at(blob, at + 4), u32_at(blob, at + 8)) else { continue };
        let n = (rows as usize).checked_mul(cols as usize).filter(|n| (1..=MAX_MESH_NODES).contains(n));
        let Some(n) = n.filter(|n| n.checked_mul(47).and_then(|b| b.checked_add(at + 12)).is_some_and(|e| e <= blob.len())) else { continue };
        let mut colors = Vec::with_capacity(n);
        let mut p = at + 12;
        for _ in 0..n {
            if u32_at(blob, p) != Some(43) {
                continue 'scan;
            }
            match color_block_at(blob, p + 4) {
                Ok((Stored::Known(c), _, end)) if end == p + 47 => colors.push(c),
                _ => continue 'scan,
            }
            p += 47;
        }
        return Some(colors);
    }
    None
}

/// The mean of colours of one model (names dropped); the first colour when models differ.
fn average(colors: &[Color]) -> Option<Color> {
    let base = |c: &Color| match c {
        Color::Named { base, .. } => (**base).clone(),
        other => other.clone(),
    };
    let all: Vec<Color> = colors.iter().map(base).collect();
    let n = all.len() as f64;
    let mut sum = [0.0f64; 4];
    for c in &all {
        let v = match (c, all.first()?) {
            (Color::Rgb { r, g, b }, Color::Rgb { .. }) => [*r, *g, *b, 0.0],
            (Color::Cmyk { c, m, y, k }, Color::Cmyk { .. }) => [*c, *m, *y, *k],
            (Color::Gray { v }, Color::Gray { .. }) => [*v, 0.0, 0.0, 0.0],
            _ => return all.first().cloned(),
        };
        for (s, x) in sum.iter_mut().zip(v) {
            *s += x;
        }
    }
    let [a, b, c, d] = sum.map(|s| s / n);
    Some(match all.first()? {
        Color::Rgb { .. } => Color::Rgb { r: a, g: b, b: c },
        Color::Cmyk { .. } => Color::Cmyk { c: a, m: b, y: c, k: d },
        Color::Gray { .. } => Color::Gray { v: a },
        other => other.clone(),
    })
}
