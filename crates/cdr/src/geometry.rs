//! Object records (`loda`) and their geometry payloads, the object transform (`trfd`) and the
//! bounds record (`bbox`). Geometry is decoded in object-local units (254 000 per inch, y up);
//! the structure module places it with the object's own transform.
//!
//! `loda` layout (all little-endian):
//!
//! ```text
//! 0   u32 length           == payload length
//! 4   u32 numArgs
//! 8   u32 offsetsStart     (20)
//! 12  u32 typesStart       (20 + 4·(numArgs+1))
//! 16  u32 objType
//!     u32 offsets[numArgs+1]   the last one == length; argument i is bytes offsets[i]..offsets[i+1]
//!     u32 typeIds[numArgs]     stored in REVERSE order: argument i has typeIds[numArgs-1-i]
//! ```

use crate::model::{Affine, Path, Point, Segment, SubPath};

/// Decoders report a short reason; the structure module turns it into a warning.
pub(crate) type Decode<T> = Result<T, &'static str>;

/// `loda` argument type ids used by the reader.
pub(crate) mod arg {
    pub(crate) const OUTLINE: u32 = 10;
    pub(crate) const FILL: u32 = 20;
    pub(crate) const GEOMETRY: u32 = 30;
    /// u32 version, u32 length, UTF-8 JSON: a cached copy of the object's paint and style.
    pub(crate) const STYLE_CACHE: u32 = 201;
    pub(crate) const NAME: u32 = 1000;
    pub(crate) const LAYER_COLOR: u32 = 2000;
    pub(crate) const TRANSPARENCY: u32 = 8000;
    pub(crate) const POWERCLIP: u32 = 8005;
    /// u32: an object style id (`stlt`), 0 when the object has its own paint.
    pub(crate) const STYLE: u32 = 12000;
    pub(crate) const WINDING: u32 = 14001;
    pub(crate) const GUIDELINE: u32 = 16000;
    /// The definition of a mesh fill (objType 32).
    pub(crate) const MESH: u32 = 19150;
    pub(crate) const LAYER_GUID: u32 = 40050;
}

/// `loda` object types (`objType`).
pub(crate) mod kind {
    pub(crate) const GROUP_OR_LAYER: u32 = 0;
    pub(crate) const RECTANGLE: u32 = 1;
    pub(crate) const ELLIPSE: u32 = 2;
    pub(crate) const CURVE: u32 = 3;
    pub(crate) const TEXT: u32 = 4;
    pub(crate) const BITMAP: u32 = 5;
    pub(crate) const GRID_LAYER: u32 = 11;
    pub(crate) const GUIDES_LAYER: u32 = 12;
    pub(crate) const DESKTOP_LAYER: u32 = 17;
    pub(crate) const GUIDELINE: u32 = 22;
    /// A property-only object without geometry, carried over from an imported file.
    pub(crate) const PLACEHOLDER: u32 = 31;
    /// An outline in arg 30 plus a definition blob (a mesh fill, by the file's own statistics).
    pub(crate) const MESH: u32 = 32;
}

/// Most arguments one `loda` may declare (the sample has at most 12).
const MAX_ARGS: usize = 4096;
/// Most points one path may hold (the sample has at most 523).
pub(crate) const MAX_PATH_POINTS: usize = 1 << 20;
/// Most records one `trfd` may compose (the sample always has one).
const MAX_TRANSFORM_PARTS: usize = 64;
/// Longest object or layer name, in UTF-16 code units.
const MAX_NAME_UNITS: usize = 1024;
/// Cubic approximation of a quarter ellipse; CorelDRAW's own bounds match this curve.
const KAPPA: f64 = 0.552_284_749_830_793_6;

fn bytes_at(b: &[u8], at: usize, n: usize) -> Option<&[u8]> {
    b.get(at..at.checked_add(n)?)
}

pub(crate) fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    let s = bytes_at(b, at, 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = bytes_at(b, at, 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub(crate) fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    u32_at(b, at).map(|v| v as i32)
}

pub(crate) fn f64_at(b: &[u8], at: usize) -> Option<f64> {
    let s = bytes_at(b, at, 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(f64::from_le_bytes(a))
}

fn index(v: u32) -> usize {
    v as usize
}

/// A decoded `loda`: its object type and argument table.
#[derive(Debug, Clone)]
pub(crate) struct Loda<'a> {
    pub(crate) obj_type: u32,
    args: Vec<(u32, &'a [u8])>,
}

impl<'a> Loda<'a> {
    pub(crate) fn parse(b: &'a [u8]) -> Decode<Self> {
        const BAD: &str = "an object record with a bad argument table";
        let length = u32_at(b, 0).ok_or("a truncated object record")?;
        if index(length) != b.len() {
            return Err("an object record of the wrong length");
        }
        let count = index(u32_at(b, 4).ok_or(BAD)?);
        if count > MAX_ARGS {
            return Err("an object record with too many arguments");
        }
        let offsets = index(u32_at(b, 8).ok_or(BAD)?);
        let types = index(u32_at(b, 12).ok_or(BAD)?);
        let obj_type = u32_at(b, 16).ok_or(BAD)?;
        let mut args = Vec::with_capacity(count);
        let mut start = index(u32_at(b, offsets).ok_or(BAD)?);
        for i in 0..count {
            let end = index(u32_at(b, offsets.checked_add(4 * (i + 1)).ok_or(BAD)?).ok_or(BAD)?);
            let ty = u32_at(b, types.checked_add(4 * (count - 1 - i)).ok_or(BAD)?).ok_or(BAD)?;
            let data = b.get(start..end).ok_or(BAD)?;
            args.push((ty, data));
            start = end;
        }
        if start != b.len() {
            return Err(BAD);
        }
        Ok(Self { obj_type, args })
    }

    pub(crate) fn arg(&self, id: u32) -> Option<&'a [u8]> {
        self.args.iter().find(|(t, _)| *t == id).map(|(_, d)| *d)
    }

    /// A 4-byte argument as u32.
    pub(crate) fn u32_arg(&self, id: u32) -> Option<u32> {
        self.arg(id).filter(|d| d.len() == 4).and_then(|d| u32_at(d, 0))
    }

    /// Arg 1000: a UTF-16LE, NUL-terminated name.
    pub(crate) fn name(&self) -> Option<String> {
        self.arg(arg::NAME).and_then(utf16z)
    }
}

/// UTF-16LE up to the first NUL (at most [`MAX_NAME_UNITS`] units; lone surrogates replaced).
/// `None` when empty.
pub(crate) fn utf16z(b: &[u8]) -> Option<String> {
    let units = b.as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p)).take_while(|u| *u != 0).take(MAX_NAME_UNITS);
    let s: String = char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect();
    (!s.trim().is_empty()).then_some(s)
}

/// `trfd`: a length/count/offset table of transform records; each kind-8 record is an affine
/// matrix `x' = a·x + b·y + c`, `y' = d·x + e·y + f` (six f64 at record + 16). Records are composed
/// in order (only one occurs in the sample). Returns the matrix and the record count.
///
/// ```text
/// 0x00 u32 length | u32 count | u32 offsetsStart (0x10) | u32 0x0000FFFF | u32 offsets[count+1]
/// record: u32 tag (0x6CC) | u32 recLen (56) | u16 kind (8) | 6 bytes | f64 a, b, c, d, e, f
/// ```
pub(crate) fn transform(b: &[u8]) -> Decode<(Affine, usize)> {
    const BAD: &str = "a damaged transform";
    let length = u32_at(b, 0).ok_or(BAD)?;
    if index(length) != b.len() {
        return Err(BAD);
    }
    let count = index(u32_at(b, 4).ok_or(BAD)?);
    if count == 0 || count > MAX_TRANSFORM_PARTS {
        return Err(BAD);
    }
    let table = index(u32_at(b, 8).ok_or(BAD)?);
    let mut m = Affine::IDENTITY;
    let mut start = index(u32_at(b, table).ok_or(BAD)?);
    for i in 0..count {
        let end = index(u32_at(b, table.checked_add(4 * (i + 1)).ok_or(BAD)?).ok_or(BAD)?);
        let record = b.get(start..end).ok_or(BAD)?;
        let len = index(u32_at(record, 4).ok_or(BAD)?);
        if len.checked_add(8).is_none_or(|l| l > record.len()) {
            return Err(BAD);
        }
        if u16_at(record, 8) != Some(8) || len < 56 {
            return Err("an unknown kind of transform");
        }
        let mut v = [0.0; 6];
        for (k, slot) in v.iter_mut().enumerate() {
            *slot = f64_at(record, 16 + 8 * k).ok_or(BAD)?;
        }
        let part = Affine::from_cdr(v);
        if !part.is_finite() {
            return Err("an invalid transform");
        }
        m = m.then(part);
        start = end;
    }
    if start != b.len() {
        return Err(BAD);
    }
    Ok((m, count))
}

/// `bbox` (and each half of `obbx`): i32 xMin, yMax, xMax, yMin in world units.
pub(crate) fn bbox(b: &[u8]) -> Decode<[i32; 4]> {
    if b.len() != 16 {
        return Err("a damaged bounding box");
    }
    let v = |k: usize| i32_at(b, 4 * k).ok_or("a damaged bounding box");
    Ok([v(0)?, v(1)?, v(2)?, v(3)?])
}

/// A path at `at`: u32 N, N × (i32 x, i32 y), N node-type bytes. Returns the path in local units
/// and the offset just past it.
///
/// Node type: bits 7–6 = 00 move, 01 line, 11 control point, 10 curve end (after two control
/// points); bit 3 on the last node of a subpath closes it. The other bits (continuity hints) do
/// not change the geometry.
pub(crate) fn path_at(b: &[u8], at: usize, max_points: usize) -> Decode<(Path, usize)> {
    let n = index(u32_at(b, at).ok_or("a truncated curve")?);
    if n > max_points {
        return Err("a curve with too many points");
    }
    let points = at.checked_add(4).ok_or("a truncated curve")?;
    let types = points.checked_add(8 * n).ok_or("a truncated curve")?;
    let end = types.checked_add(n).ok_or("a truncated curve")?;
    let node_types = b.get(types..end).ok_or("a truncated curve")?;
    let point = |i: usize| -> Decode<Point> {
        let x = i32_at(b, points + 8 * i).ok_or("a truncated curve")?;
        let y = i32_at(b, points + 8 * i + 4).ok_or("a truncated curve")?;
        Ok(Point::new(f64::from(x), f64::from(y)))
    };
    let kind = |i: usize| node_types.get(i).map(|t| t >> 6);
    let mut subpaths = Vec::new();
    let mut current: Option<SubPath> = None;
    let mut ended = false;
    let mut i = 0;
    while i < n {
        let t = node_types.get(i).copied().ok_or("a truncated curve")?;
        match t >> 6 {
            0 => {
                if let Some(s) = current.take().filter(|s| !s.segments.is_empty()) {
                    subpaths.push(s);
                }
                current = Some(SubPath { start: point(i)?, segments: Vec::new(), closed: false });
                ended = false;
                i += 1;
            }
            1 => {
                let s = current.as_mut().ok_or("a curve segment before its first point")?;
                if ended {
                    return Err("a curve segment after the end of its subpath");
                }
                s.segments.push(Segment::Line(point(i)?));
                if t & 0x08 != 0 {
                    s.closed = true;
                    ended = true;
                }
                i += 1;
            }
            3 => {
                let s = current.as_mut().ok_or("a curve segment before its first point")?;
                if ended {
                    return Err("a curve segment after the end of its subpath");
                }
                if kind(i + 1) != Some(3) || kind(i + 2) != Some(2) {
                    return Err("a malformed Bézier segment");
                }
                s.segments.push(Segment::Cubic(point(i)?, point(i + 1)?, point(i + 2)?));
                if node_types.get(i + 2).is_some_and(|t| t & 0x08 != 0) {
                    s.closed = true;
                    ended = true;
                }
                i += 3;
            }
            _ => return Err("a curve point without control points"),
        }
    }
    if let Some(s) = current.filter(|s| !s.segments.is_empty()) {
        subpaths.push(s);
    }
    Ok((Path { subpaths }, end))
}

/// Curve payload (objType 3 and 32): exactly one path.
pub(crate) fn curve(b: &[u8], max_points: usize) -> Decode<Path> {
    let (path, end) = path_at(b, 0, max_points)?;
    if end != b.len() {
        return Err("a curve record of the wrong length");
    }
    Ok(path)
}

/// Rectangle payload (objType 1, 128 bytes): f64 width, f64 height (the local box spans (0,0) to
/// (w,h)), f64 scaleX, f64 scaleY, then four 24-byte corners {i32 type, 4 bytes, f64 radius
/// fraction, i32, 4 bytes}. A corner radius is `fraction · min(|w|,|h|) / 2`. Returns the outline
/// and whether it used corner shapes nobody verified (scalloped, chamfered or unknown types).
pub(crate) fn rectangle(b: &[u8]) -> Decode<(Path, bool)> {
    if b.len() != 128 {
        return Err("a rectangle record of the wrong length");
    }
    let w = f64_at(b, 0).ok_or("a truncated rectangle")?;
    let h = f64_at(b, 8).ok_or("a truncated rectangle")?;
    if !w.is_finite() || !h.is_finite() || w.abs() > 1e12 || h.abs() > 1e12 {
        return Err("a rectangle with an invalid size");
    }
    let limit = w.abs().min(h.abs()) / 2.0;
    let mut radii = [0.0; 4];
    let mut types = [0i32; 4];
    for c in 0..4 {
        let at = 32 + 24 * c;
        types[c] = i32_at(b, at).ok_or("a truncated rectangle")?;
        let fraction = f64_at(b, at + 8).ok_or("a truncated rectangle")?;
        radii[c] = if fraction.is_finite() { (fraction * limit).clamp(0.0, limit) } else { 0.0 };
    }
    let unverified = (0..4).any(|c| radii[c] > 0.0 && types[c] != 0);
    let p = Point::new;
    if radii.iter().all(|r| *r == 0.0) {
        let segments = vec![Segment::Line(p(w, 0.0)), Segment::Line(p(w, h)), Segment::Line(p(0.0, h))];
        return Ok((Path { subpaths: vec![SubPath { start: p(0.0, 0.0), segments, closed: true }] }, false));
    }
    // Corner order assumed (0,0), (w,0), (w,h), (0,h); the sample's corners are all equal.
    let (sx, sy) = (if w < 0.0 { -1.0 } else { 1.0 }, if h < 0.0 { -1.0 } else { 1.0 });
    let corners = [(0.0, 0.0, 0.0, sy, sx, 0.0), (w, 0.0, -sx, 0.0, 0.0, sy), (w, h, 0.0, -sy, -sx, 0.0), (0.0, h, sx, 0.0, 0.0, -sy)];
    let mut start = None;
    let mut segments = Vec::new();
    for (c, (px, py, ax, ay, bx, by)) in corners.into_iter().enumerate() {
        let r = radii[c];
        let s = p(px + ax * r, py + ay * r);
        let e = p(px + bx * r, py + by * r);
        match start {
            None => start = Some(s),
            Some(_) => segments.push(Segment::Line(s)),
        }
        if r == 0.0 {
            continue;
        }
        let k = r * KAPPA;
        segments.push(match types[c] {
            // Chamfer (inferred): a straight cut.
            2 => Segment::Line(e),
            // Scallop (inferred): a concave quarter circle centred on the corner.
            1 => Segment::Cubic(p(s.x + bx * k, s.y + by * k), p(e.x + ax * k, e.y + ay * k), e),
            _ => Segment::Cubic(p(s.x - ax * k, s.y - ay * k), p(e.x - bx * k, e.y - by * k), e),
        });
    }
    let start = start.unwrap_or_default();
    Ok((Path { subpaths: vec![SubPath { start, segments, closed: true }] }, unverified))
}

/// Ellipse payload (objType 2, 20 bytes): i32 width, i32 height (local box (0,0) to (w,h), centre
/// at its middle), i32 start and end angle in micro-degrees, u32 arc kind. Equal angles make a
/// full ellipse; otherwise (unverified: no sample) arc kind 2 is an open arc and anything else a
/// pie, with angles counter-clockwise from +x. Returns the outline and whether it is an arc or pie.
pub(crate) fn ellipse(b: &[u8]) -> Decode<(Path, bool)> {
    if b.len() != 20 {
        return Err("an ellipse record of the wrong length");
    }
    let field = |k: usize| i32_at(b, 4 * k).ok_or("a truncated ellipse");
    let (w, h, start, end) = (f64::from(field(0)?), f64::from(field(1)?), field(2)?, field(3)?);
    let arc_kind = u32_at(b, 16).ok_or("a truncated ellipse")?;
    let (cx, cy, rx, ry) = (w / 2.0, h / 2.0, w / 2.0, h / 2.0);
    let a0 = f64::from(start) / 1e6;
    let full = (i64::from(end) - i64::from(start)).rem_euclid(360_000_000) == 0;
    if full {
        let (s, segments) = arc(cx, cy, rx, ry, a0, 360.0);
        return Ok((Path { subpaths: vec![SubPath { start: s, segments, closed: true }] }, false));
    }
    let sweep = (f64::from(end) / 1e6 - a0).rem_euclid(360.0);
    let sweep = if sweep <= 0.0 { 360.0 } else { sweep };
    let (s, mut segments) = arc(cx, cy, rx, ry, a0, sweep);
    let closed = arc_kind != 2;
    if closed {
        segments.push(Segment::Line(Point::new(cx, cy)));
    }
    Ok((Path { subpaths: vec![SubPath { start: s, segments, closed }] }, true))
}

/// An elliptical arc from `a0` degrees over `sweep` degrees (0 < sweep <= 360) in cubic pieces of
/// at most 90°.
fn arc(cx: f64, cy: f64, rx: f64, ry: f64, a0: f64, sweep: f64) -> (Point, Vec<Segment>) {
    let pieces = (sweep / 90.0 - 1e-9).ceil().clamp(1.0, 4.0);
    let step = (sweep / pieces).to_radians();
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let at = |t: f64| Point::new(cx + rx * t.cos(), cy + ry * t.sin());
    let tangent = |t: f64| Point::new(-rx * t.sin(), ry * t.cos());
    let mut t = a0.to_radians();
    let start = at(t);
    let mut segments = Vec::with_capacity(4);
    for _ in 0..pieces as usize {
        let t2 = t + step;
        let (p0, p3, d0, d3) = (at(t), at(t2), tangent(t), tangent(t2));
        segments.push(Segment::Cubic(Point::new(p0.x + k * d0.x, p0.y + k * d0.y), Point::new(p3.x - k * d3.x, p3.y - k * d3.y), p3));
        t = t2;
    }
    (start, segments)
}

/// Bitmap object payload (objType 5).
///
/// ```text
/// 0   i32 xLeft, yTop, xRight, yBottom   frame (local units): where the picture is placed
/// 16  i32 × 4                            a second copy of the frame
/// 32  u16 (2)  34 u16 bitsPerPixel  36 i32 widthPx  40 i32 heightPx  44 i32 (unknown)
/// 48  i32 bitmapId                       the `bmp ` record whose first u32 is this id
/// 56  i32 crop flag                      1: the clip path is a real crop, 0: it traces the frame
/// 72  path                               clip outline (local units), then u32 0
/// ```
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BitmapRecord {
    pub(crate) frame: [i32; 4],
    pub(crate) bits_per_pixel: u16,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) bitmap_id: u32,
    pub(crate) cropped: bool,
    /// Clip outline in local units.
    pub(crate) clip: Path,
}

pub(crate) fn bitmap(b: &[u8], max_points: usize) -> Decode<BitmapRecord> {
    const SHORT: &str = "a truncated bitmap record";
    if b.len() < 72 {
        return Err(SHORT);
    }
    let field = |at| i32_at(b, at).ok_or(SHORT);
    let frame = [field(0)?, field(4)?, field(8)?, field(12)?];
    let (clip, end) = path_at(b, 72, max_points)?;
    if end.checked_add(4) != Some(b.len()) {
        return Err("a bitmap record of the wrong length");
    }
    Ok(BitmapRecord {
        frame,
        bits_per_pixel: u16_at(b, 34).ok_or(SHORT)?,
        width: field(36)?,
        height: field(40)?,
        bitmap_id: u32_at(b, 48).ok_or(SHORT)?,
        cropped: field(56)? != 0,
        clip,
    })
}

impl BitmapRecord {
    /// Pixel space (0..w × 0..h, y down, pixel (0,0) at the frame corner (xLeft, yTop)) to local
    /// units. `None` for an empty frame or picture.
    pub(crate) fn pixel_to_local(&self) -> Option<Affine> {
        let [x0, top, x1, bottom] = self.frame.map(f64::from);
        if self.width <= 0 || self.height <= 0 || x1 == x0 || top == bottom {
            return None;
        }
        let sx = (x1 - x0) / f64::from(self.width);
        let sy = (top - bottom) / f64::from(self.height);
        Some(Affine([sx, 0.0, 0.0, -sy, x0, top]))
    }
}

/// Text object payload (objType 4, 8 bytes): u32 text kind (0 = artistic), u32 story id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextRecord {
    pub(crate) text_kind: u32,
    pub(crate) story: u32,
}

pub(crate) fn text(b: &[u8]) -> Decode<TextRecord> {
    if b.len() != 8 {
        return Err("a text record of the wrong length");
    }
    Ok(TextRecord { text_kind: u32_at(b, 0).ok_or("a truncated text record")?, story: u32_at(b, 4).ok_or("a truncated text record")? })
}

/// Guideline (objType 22, arg 16000, 40 bytes): 3 × u32, i32 angle (micro-degrees), then three
/// points; the line runs through the first two (world units). Returns two distinct points.
pub(crate) fn guideline(b: &[u8]) -> Decode<(Point, Point)> {
    if b.len() != 40 {
        return Err("a guideline record of the wrong length");
    }
    let field = |at| i32_at(b, at).map(f64::from).ok_or("a truncated guideline");
    let angle = field(12)? / 1e6;
    let a = Point::new(field(16)?, field(20)?);
    let b2 = Point::new(field(24)?, field(28)?);
    if a != b2 {
        return Ok((a, b2));
    }
    let r = angle.to_radians();
    Ok((a, Point::new(a.x + 1000.0 * r.cos(), a.y + 1000.0 * r.sin())))
}
