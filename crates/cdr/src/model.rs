//! The document model VectorCraft imports, in the reader's own types.
//!
//! Every coordinate is in points (1/72 inch) with y pointing **down** and the origin at the
//! top-left corner of the page rectangle. CorelDRAW stores 254 000 units per inch with y up and
//! the page centred on the origin; the reader converts once, so every path, transform and
//! gradient here is already in page space. Children are listed bottom to top (CorelDRAW lists the
//! front-most object first; the reader reverses that). Anything not imported is reported in
//! [`Document::warnings`] instead of being dropped silently.

use std::collections::BTreeMap;
use std::sync::Arc;

/// CorelDRAW world units per inch (1 unit = 0.1 µm).
pub const UNITS_PER_INCH: f64 = 254_000.0;
/// Points per CorelDRAW world unit.
pub const POINTS_PER_UNIT: f64 = 72.0 / UNITS_PER_INCH;

/// Affine map `x' = a·x + c·y + e`, `y' = b·x + d·y + f` (kurbo's coefficient order `[a, b, c, d, e, f]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine(pub [f64; 6]);

impl Affine {
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    /// CorelDRAW's row-major matrix `[A, B, C, D, E, F]` with `x' = A·x + B·y + C`, `y' = D·x + E·y + F`.
    pub(crate) fn from_cdr(m: [f64; 6]) -> Self {
        Self([m[0], m[3], m[1], m[4], m[2], m[5]])
    }

    pub fn scale_non_uniform(sx: f64, sy: f64) -> Self {
        Self([sx, 0.0, 0.0, sy, 0.0, 0.0])
    }

    pub fn translate(x: f64, y: f64) -> Self {
        Self([1.0, 0.0, 0.0, 1.0, x, y])
    }

    /// `self` first, then `outer`.
    pub fn then(self, outer: Self) -> Self {
        let [a, b, c, d, e, f] = self.0;
        let [oa, ob, oc, od, oe, of] = outer.0;
        Self([oa * a + oc * b, ob * a + od * b, oa * c + oc * d, ob * c + od * d, oa * e + oc * f + oe, ob * e + od * f + of])
    }

    pub fn apply(&self, p: Point) -> Point {
        let [a, b, c, d, e, f] = self.0;
        Point { x: a * p.x + c * p.y + e, y: b * p.x + d * p.y + f }
    }

    pub fn determinant(&self) -> f64 {
        let [a, b, c, d, ..] = self.0;
        a * d - b * c
    }

    /// Geometric mean of the axis scales: how much a stroke width or font size grows.
    pub fn scale(&self) -> f64 {
        self.determinant().abs().sqrt()
    }

    /// The largest factor by which the map stretches a length (the larger singular value of
    /// the linear part).
    pub fn max_scale(&self) -> f64 {
        let [a, b, c, d, ..] = self.0;
        let sum = a * a + b * b + c * c + d * d;
        let root = (((a - d).powi(2) + (b + c).powi(2)) * ((a + d).powi(2) + (b - c).powi(2))).sqrt();
        ((sum + root) / 2.0).sqrt()
    }

    /// The inverse map, `None` when the map is singular or not finite.
    pub fn inverse(&self) -> Option<Self> {
        let [a, b, c, d, e, f] = self.0;
        let det = self.determinant();
        if !det.is_finite() || det.abs() < 1e-300 {
            return None;
        }
        let m = Self([d / det, -b / det, -c / det, a / det, (c * f - d * e) / det, (b * e - a * f) / det]);
        m.is_finite().then_some(m)
    }

    pub fn is_finite(&self) -> bool {
        self.0.iter().all(|v| v.is_finite())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle, `x0 <= x1` and `y0 <= y1`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    /// The smallest rectangle holding both points.
    pub fn from_points(a: Point, b: Point) -> Self {
        Self { x0: a.x.min(b.x), y0: a.y.min(b.y), x1: a.x.max(b.x), y1: a.y.max(b.y) }
    }

    pub fn union(self, o: Self) -> Self {
        Self { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }

    pub fn width(&self) -> f64 {
        self.x1 - self.x0
    }

    pub fn height(&self) -> f64 {
        self.y1 - self.y0
    }

    /// The bounds of this rectangle's four corners mapped through `m`.
    pub fn transformed(&self, m: &Affine) -> Self {
        let corners = [Point::new(self.x0, self.y0), Point::new(self.x1, self.y0), Point::new(self.x1, self.y1), Point::new(self.x0, self.y1)];
        let mut r = Rect::from_points(m.apply(corners[0]), m.apply(corners[2]));
        for p in [corners[1], corners[3]] {
            let q = m.apply(p);
            r = r.union(Rect { x0: q.x, y0: q.y, x1: q.x, y1: q.y });
        }
        r
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    Line(Point),
    /// Control 1, control 2, end.
    Cubic(Point, Point, Point),
}

impl Segment {
    pub fn end(&self) -> Point {
        match self {
            Segment::Line(p) | Segment::Cubic(_, _, p) => *p,
        }
    }

    fn map(&self, m: &Affine) -> Self {
        match self {
            Segment::Line(p) => Segment::Line(m.apply(*p)),
            Segment::Cubic(a, b, c) => Segment::Cubic(m.apply(*a), m.apply(*b), m.apply(*c)),
        }
    }
}

/// One subpath. `closed` adds a closing line back to `start` (often of length zero: CorelDRAW
/// usually ends a closed subpath on its start point).
#[derive(Debug, Clone, PartialEq)]
pub struct SubPath {
    pub start: Point,
    pub segments: Vec<Segment>,
    pub closed: bool,
}

/// An outline in page space.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Path {
    pub subpaths: Vec<SubPath>,
}

impl Path {
    pub fn transformed(&self, m: &Affine) -> Self {
        Self {
            subpaths: self
                .subpaths
                .iter()
                .map(|s| SubPath { start: m.apply(s.start), segments: s.segments.iter().map(|g| g.map(m)).collect(), closed: s.closed })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.subpaths.is_empty()
    }

    /// Every stored point is finite and within `limit` of the origin on both axes.
    pub fn within(&self, limit: f64) -> bool {
        let ok = |p: &Point| p.x.is_finite() && p.y.is_finite() && p.x.abs() <= limit && p.y.abs() <= limit;
        self.subpaths.iter().all(|s| {
            ok(&s.start)
                && s.segments.iter().all(|g| match g {
                    Segment::Line(p) => ok(p),
                    Segment::Cubic(a, b, c) => ok(a) && ok(b) && ok(c),
                })
        })
    }

    /// Stored points (on-curve points and control points).
    pub fn point_count(&self) -> usize {
        self.subpaths.iter().map(|s| 1 + s.segments.iter().map(|g| if matches!(g, Segment::Line(_)) { 1 } else { 3 }).sum::<usize>()).sum()
    }

    /// Tight bounds, using the extrema of each cubic (not its control points).
    pub fn bounds(&self) -> Option<Rect> {
        let mut r: Option<Rect> = None;
        let mut add = |p: Point| {
            let b = Rect { x0: p.x, y0: p.y, x1: p.x, y1: p.y };
            r = Some(r.map_or(b, |r| r.union(b)));
        };
        for s in &self.subpaths {
            add(s.start);
            let mut cur = s.start;
            for g in &s.segments {
                match g {
                    Segment::Line(p) => add(*p),
                    Segment::Cubic(c1, c2, p) => {
                        add(*p);
                        for t in cubic_extrema(cur.x, c1.x, c2.x, p.x).into_iter().chain(cubic_extrema(cur.y, c1.y, c2.y, p.y)).flatten() {
                            add(cubic_at(cur, *c1, *c2, *p, t));
                        }
                    }
                }
                cur = g.end();
            }
        }
        r
    }
}

fn cubic_at(p0: Point, p1: Point, p2: Point, p3: Point, t: f64) -> Point {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    Point { x: a * p0.x + b * p1.x + c * p2.x + d * p3.x, y: a * p0.y + b * p1.y + c * p2.y + d * p3.y }
}

/// Parameters in (0, 1) where one coordinate of a cubic has a local extremum.
fn cubic_extrema(p0: f64, p1: f64, p2: f64, p3: f64) -> [Option<f64>; 2] {
    let a = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
    let b = 2.0 * (p0 - 2.0 * p1 + p2);
    let c = p1 - p0;
    let inside = |t: f64| (t > 0.0 && t < 1.0 && t.is_finite()).then_some(t);
    if a.abs() < 1e-12 {
        if b.abs() > 1e-12 {
            return [inside(-c / b), None];
        }
        return [None, None];
    }
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return [None, None];
    }
    let s = disc.sqrt();
    [inside((-b + s) / (2.0 * a)), inside((-b - s) / (2.0 * a))]
}

/// A document: its pages in order, the format version and the import warnings.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    /// Format version from the `vrsn` record (2600 = major 26, minor 0), 0 when absent.
    pub version: u32,
    pub pages: Vec<Page>,
    /// Content that was not imported or was approximated, one line per kind, deduplicated
    /// (`"message (n×)"` when it happened n > 1 times).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// "Page 1", "Page 2"…: CorelDRAW pages in the sample carry no stored name.
    pub name: String,
    /// Page size in points.
    pub width: f64,
    pub height: f64,
    /// Bottom to top.
    pub layers: Vec<Layer>,
    /// Guidelines shown on this page (the master page's and the page's own).
    pub guides: Vec<Guide>,
    /// Bounds of everything on the page (CorelDRAW's page `bbox`), which may reach far beyond
    /// the page rectangle onto the desktop.
    pub bounds: Option<Rect>,
}

/// An infinite guideline through two points, in page space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    pub a: Point,
    pub b: Point,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub name: String,
    pub visible: bool,
    pub locked: bool,
    pub printable: bool,
    /// A master-page layer, repeated on every page that shows it.
    pub master: bool,
    /// The layer's display colour in CorelDRAW's object manager.
    pub color: Option<Color>,
    /// Bottom to top.
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Object name; CorelDRAW stores none for most objects.
    pub name: Option<String>,
    pub visible: bool,
    pub locked: bool,
    /// Uniform opacity 0..1 (CorelDRAW's uniform transparency, inverted).
    pub opacity: f64,
    pub blend: Blend,
    /// A transparency ramp, to be applied as an opacity mask over the node.
    pub opacity_mask: Option<OpacityMask>,
    /// The bounds CorelDRAW stored for the object (its `bbox`) in page space: geometric, without
    /// the outline width. For a text object it is the ink box of the laid-out glyphs, which the
    /// reader cannot compute itself; for a PowerClip it is the frame alone.
    pub bounds: Option<Rect>,
    pub kind: Kind,
}

impl Node {
    pub(crate) fn new(kind: Kind) -> Self {
        Self { name: None, visible: true, locked: false, opacity: 1.0, blend: Blend::Normal, opacity_mask: None, bounds: None, kind }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// Groups and link groups (the generated part of a contour or blend). Bottom to top.
    Group {
        children: Vec<Node>,
    },
    Shape {
        path: Path,
        fill: Paint,
        outline: Option<Outline>,
        /// Even-odd fill rule (CorelDRAW's default for curves); otherwise nonzero.
        even_odd: bool,
        /// The outline is painted behind the fill.
        outline_behind: bool,
    },
    Text(Text),
    Image(Image),
    /// A PowerClip: `frame` (normally a shape, with its own fill and outline) clips `contents`.
    /// The frame paints below the contents.
    PowerClip {
        frame: Box<Node>,
        /// Bottom to top, already in page space.
        contents: Vec<Node>,
        /// The contents move with the frame (CorelDRAW's "lock contents to PowerClip").
        contents_locked: bool,
    },
}

/// Blend modes of CorelDRAW transparency (its "merge modes"). Only two ids occur in the files the
/// reader was checked on: 0 (normal) and one that is drawn as [`Blend::Overlay`] (an approximation
/// a warning reports). Every other id comes in as `Normal` with a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Blend {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Paint {
    None,
    Solid(Color),
    Gradient(Gradient),
    /// A paint this reader does not decode (pattern, texture, PostScript…), with the reason. A
    /// warning names it too; callers normally draw nothing.
    Unsupported(&'static str),
}

/// A colour as stored, components 0..1. CMYK stays CMYK: the reader does no colour management
/// (CorelDRAW converts process colours through the document's CMYK profile, which the reader
/// deliberately ignores; the caller converts with its own).
#[derive(Debug, Clone, PartialEq)]
pub enum Color {
    Rgb {
        r: f64,
        g: f64,
        b: f64,
    },
    Cmyk {
        c: f64,
        m: f64,
        y: f64,
        k: f64,
    },
    /// Grey level, 0 black .. 1 white.
    Gray {
        v: f64,
    },
    /// CIE L*a*b*: `l` 0..100, `a`/`b` about −128..127.
    Lab {
        l: f64,
        a: f64,
        b: f64,
    },
    /// A colour picked from a named palette entry (a swatch): `tint` 0..1 of `base`, the entry's
    /// process or RGB value. `spot` marks a spot ink that separates on its own plate; otherwise
    /// the name is only the swatch's label. The reader names a colour when it carries the id of
    /// the document palette (`color/docPalette.xml`) and that palette has an entry of the same
    /// value; spot colour palettes are not decoded yet (no sample), so `spot` is always false.
    Named {
        name: String,
        palette: Option<String>,
        spot: bool,
        tint: f64,
        base: Box<Color>,
    },
}

/// CorelDRAW's fountain fill types. Linear and radial were verified on a sample; conical and
/// square were not (no sample) and come with a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientKind {
    Linear,
    /// Elliptical: distance from the centre in gradient space.
    Radial,
    /// Angle around the centre.
    Conical,
    /// Distance from the centre along the larger axis (CorelDRAW's square fountain).
    Square,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    /// 0..1 along the gradient, never decreasing from one stop to the next.
    pub offset: f64,
    pub color: Color,
    /// Opacity of the colour 0..1.
    pub opacity: f64,
    /// Where the blend to the next stop is half way, 0..1 (0.5 = even). The exact curve of an
    /// uneven blend is not known (`w = f^(ln 0.5 / ln m)` is a reasonable rendering).
    pub midpoint: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gradient {
    pub kind: GradientKind,
    pub stops: Vec<Stop>,
    /// Gradient space → page space. In gradient space a linear gradient runs from (0, 0)
    /// (offset 0) to (1, 0) (offset 1), constant along y; radial, conical and square gradients
    /// are centred on (0, 0) with radius 1, a conical one starting at angle 0. The map may skew
    /// and scale unevenly (CorelDRAW places fountains on the object's own box), so a renderer
    /// that only takes a start and an end point loses that; outside the ramp the end colours
    /// extend (pad).
    pub transform: Affine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Butt,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    Miter,
    Round,
    Bevel,
}

/// A visible outline (an object whose pen is switched off, or has no width, has none).
#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    /// Width in points, in page space (any object scaling already applied: with
    /// `scale_with_object` the stored width is multiplied by the object's largest stretch).
    pub width: f64,
    pub paint: Paint,
    /// Always `Butt` for now: the pen's cap, dash and arrowhead fields are not decoded (a
    /// warning says so when they are set).
    pub cap: Cap,
    pub join: Join,
    /// Miter limit as a ratio of miter length to width (CorelDRAW stores an angle).
    pub miter_limit: f64,
    /// Dash and gap lengths in points. Always `None` for now (see `cap`).
    pub dash: Option<Vec<f64>>,
    pub behind_fill: bool,
    pub scale_with_object: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
    Justify,
}

/// A run of characters with one set of attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub text: String,
    /// Family name as CorelDRAW stores it; the font program itself is never in the file.
    pub family: String,
    pub postscript_name: Option<String>,
    /// Em size in text space (any uniform scale of the object is folded in).
    pub size_pt: f64,
    /// CSS weight (400 regular, 700 bold).
    pub weight: u16,
    pub italic: bool,
    pub underline: bool,
    /// Extra space after each character, in em. CorelDRAW stores character spacing as a
    /// fraction of the font's space width; the reader assumes a space of 0.3 em (inferred).
    pub tracking: f64,
    /// In page space, like every paint (a fountain is placed on the text's stored ink box).
    pub fill: Paint,
    /// Width in page points, like every outline.
    pub outline: Option<Outline>,
}

/// Artistic text. Text space has its origin at the start of the first baseline, x to the right
/// and y down, in points; `transform` maps it to page space with upright glyphs. A uniform scale
/// of the object is folded into the run sizes, so `transform` keeps only rotation, skew and
/// uneven scaling. Lines are separated by `'\n'` in the run text.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    /// Empty when the characters could not be decoded (a warning says so).
    pub runs: Vec<Run>,
    pub align: Align,
    pub transform: Affine,
}

/// A bitmap object. Cropping is a dedicated field rather than a PowerClip wrapper: `clip` is the
/// crop outline, filled with the even-odd rule, and the picture shows only inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA, 8 bits per channel, top row first: `width * height * 4` bytes. Every
    /// object that shows the same picture shares one buffer (`Arc::ptr_eq` tells a caller that it
    /// can reuse what it made of the first).
    pub rgba: Arc<Vec<u8>>,
    /// Pixel space (0..width, 0..height, y down) → page space.
    pub transform: Affine,
    /// Crop outline in page space, when the image is cropped to more than its frame.
    pub clip: Option<Path>,
}

/// Object transparency as decoded (CorelDRAW stores see-through amounts; these are opacities).
/// It applies to the whole object, fill and outline together.
#[derive(Debug, Clone, PartialEq)]
pub enum Transparency {
    None,
    /// Uniform opacity 0..1 (becomes [`Node::opacity`]).
    Uniform(f64),
    /// A ramp of opacities placed like a gradient (becomes [`Node::opacity_mask`]).
    Gradient(OpacityMask),
    /// A kind this reader does not decode, with the reason.
    Unsupported(&'static str),
}

/// A transparency ramp: draw the node through a mask whose opacity follows the stops. Outside the
/// ramp the end opacities extend. The node itself is not otherwise faded.
#[derive(Debug, Clone, PartialEq)]
pub struct OpacityMask {
    pub kind: GradientKind,
    pub stops: Vec<OpacityStop>,
    /// Gradient space → page space, as for [`Gradient::transform`].
    pub transform: Affine,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpacityStop {
    pub offset: f64,
    pub opacity: f64,
    pub midpoint: f64,
}

/// Deduplicated warnings: one line per kind with a count.
#[derive(Debug, Default)]
pub(crate) struct Warnings(BTreeMap<String, usize>);

impl Warnings {
    pub(crate) fn warn(&mut self, what: impl Into<String>) {
        *self.0.entry(what.into()).or_default() += 1;
    }

    pub(crate) fn finish(self) -> Vec<String> {
        self.0.into_iter().map(|(w, n)| if n > 1 { format!("{w} ({n}×)") } else { w }).collect()
    }
}
