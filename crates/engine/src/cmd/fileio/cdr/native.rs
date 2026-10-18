//! A CorelDRAW document read by the native reader (`vectorcraft-cdr`) as editable VectorCraft
//! art: one artboard per page, the art layers, groups, curves with their fills and outlines,
//! fountain fills as gradients, transparency (uniform, ramps as opacity masks, merge modes),
//! artistic text, bitmaps (cropped ones in a clip group) and PowerClips, which become the user's
//! own editable PowerClips, exactly as `object.powerClip.make` makes them. Named palette colours
//! become global swatches. What the reader approximated or left out comes back as its warnings;
//! what this mapping approximates is added to them.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;

use kurbo::{Affine, BezPath, Point, Rect};
use vectorcraft_cdr::model as cd;
use vectorcraft_color::{BlendMode, Color, Gradient, GradientGeom, GradientKind, GradientPaint, GradientStop, Paint, Swatch};
use vectorcraft_doc::appearance::{Appearance, AppearanceItem, Dash, FillLayer, LineCap, LineJoin, StrokeLayer};
use vectorcraft_doc::geom::{FillRule, PathData, shapes};
use vectorcraft_doc::text::{CharStyle, Justify, TextObject, TextRun};
use vectorcraft_doc::{Artboard, ColorMode, Document, Guide, ImageObject, LayerColor, Node, NodeKind, OpacityMask, PowerClipSpec};

use crate::cmd::powerclip::{is_frame, mark_frame};

/// Space between pages laid side by side, in points.
const PAGE_GAP: f64 = 36.0;
/// Most names tried for a swatch whose name is taken by a different colour.
const MAX_NAME_TRIES: usize = 1000;

/// Why the native document could not be opened (the reader's error, or a document without pages).
pub(super) type Failure = String;

/// Read `bytes` natively → the document and its warnings (the reader's, then this mapping's).
pub(super) fn import(bytes: &[u8]) -> Result<(Document, Vec<String>), Failure> {
    let d = vectorcraft_cdr::read(bytes, vectorcraft_cdr::Limits::default()).map_err(|e| e.to_string())?;
    build(&d)
}

/// The VectorCraft document of a CorelDRAW document the reader returned.
pub(super) fn build(d: &cd::Document) -> Result<(Document, Vec<String>), Failure> {
    let mut b =
        Builder { doc: Document::new(0.0, 0.0), warnings: Vec::new(), layer: 0, named: Vec::new(), cmyk: 0, rgb: 0, pictures: HashMap::new() };
    b.document(d)?;
    b.finish();
    b.doc.limit_depth_noting(&mut b.warnings);
    let mut warnings = d.warnings.iter().map(|w| format!("CorelDRAW: not imported or approximated: {w}.")).collect::<Vec<_>>();
    warnings.extend(b.warnings.iter().map(|w| format!("CorelDRAW: {w}.")));
    Ok((b.doc, warnings))
}

/// A picture stored in `Document::images`, made once for every object that shows it.
#[derive(Clone)]
struct Picture {
    key: String,
    width: u32,
    height: u32,
}

struct Builder {
    doc: Document,
    warnings: Vec<String>,
    /// Layers made so far (their colours cycle).
    layer: u8,
    /// Swatches for the file's named colours, added after the default ones.
    named: Vec<Swatch>,
    /// Process colours seen in each model: the document opens in CMYK when most are CMYK.
    cmyk: usize,
    rgb: usize,
    /// Pictures made so far, by the reader's shared pixel buffer (kept alive so its address stays
    /// unique): a picture shown by several objects is encoded once.
    pictures: HashMap<usize, (Arc<Vec<u8>>, Option<Picture>)>,
}

fn affine(a: cd::Affine) -> Affine {
    Affine::new(a.0)
}

fn point(p: cd::Point) -> Point {
    Point::new(p.x, p.y)
}

fn finite_rect(r: Rect) -> bool {
    [r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite())
}

/// A 0..1 component, 0 when it isn't a number.
fn unit(v: f64) -> f32 {
    if v.is_finite() { v.clamp(0.0, 1.0) as f32 } else { 0.0 }
}

impl Builder {
    fn warn(&mut self, w: &str) {
        if !self.warnings.iter().any(|x| x == w) {
            self.warnings.push(w.to_string());
        }
    }

    fn document(&mut self, d: &cd::Document) -> Result<(), Failure> {
        let multi = d.pages.len() > 1;
        let mut layers = Vec::new();
        let mut boards: Vec<(String, Rect)> = Vec::new();
        let mut guides = Vec::new();
        let mut seen_guides = HashSet::new();
        // Pages sit side by side, PAGE_GAP apart, each page's art (which may reach far past its
        // page onto the desktop) clear of the next page's; the first page keeps its origin.
        let mut right = 0.0f64;
        for (i, page) in d.pages.iter().enumerate() {
            let paper = Rect::new(0.0, 0.0, page.width, page.height);
            if !finite_rect(paper) || page.width <= 0.0 || page.height <= 0.0 {
                self.warn("pages without a usable size were left out");
                continue;
            }
            // The page's stored bbox may be stale or missing: the objects' own bounds count too.
            let objects = page.layers.iter().flat_map(|l| &l.nodes).filter_map(|n| n.bounds);
            let extent = page
                .bounds
                .into_iter()
                .chain(objects)
                .map(|r| Rect::new(r.x0, r.y0, r.x1, r.y1))
                .filter(|r| finite_rect(*r))
                .fold(paper, |acc, r| acc.union(r));
            let dx = if boards.is_empty() { 0.0 } else { right + PAGE_GAP - extent.x0 };
            right = dx + extent.x1;
            let to_doc = Affine::translate((dx, 0.0));
            let name = if page.name.trim().is_empty() { format!("Page {}", i + 1) } else { page.name.clone() };
            boards.push((name.clone(), to_doc.transform_rect_bbox(paper)));
            let board_id = boards.len() as u32;
            let page_layers: Vec<Arc<Node>> = page.layers.iter().map(|l| self.layer(l, to_doc)).collect();
            if multi {
                // CorelDRAW's object manager lists layers under their page: a layer per page holds
                // them as sublayers, so placing page N takes that page's art alone.
                let holder = self.layer_with(&name, page_layers);
                layers.push(Arc::new(holder));
            } else {
                layers.extend(page_layers);
            }
            for g in &page.guides {
                let (a, b) = (to_doc * point(g.a), to_doc * point(g.b));
                if ![a.x, a.y, b.x, b.y].iter().all(|v| v.is_finite()) {
                    continue;
                }
                let (vertical, pos) = if (a.x - b.x).abs() <= 1e-6 * (1.0 + a.x.abs()) {
                    (true, a.x)
                } else if (a.y - b.y).abs() <= 1e-6 * (1.0 + a.y.abs()) {
                    (false, a.y)
                } else {
                    self.warn("slanted guidelines were left out (ruler guides are horizontal or vertical)");
                    continue;
                };
                // On a multi-page document each page's guides stay on its artboard.
                let artboard = multi.then_some(board_id);
                if seen_guides.insert((vertical, pos.to_bits(), artboard)) {
                    guides.push(Guide { vertical, pos, artboard, layer: None });
                }
            }
        }
        if boards.is_empty() {
            return Err("the CorelDRAW document has no page with a size".into());
        }
        self.doc.artboards = boards
            .into_iter()
            .enumerate()
            .map(|(i, (name, rect))| Artboard { id: i as u32 + 1, name, rect, show_center_mark: false, show_cross_hairs: false })
            .collect();
        // Bottom to top, as the reader lists them.
        self.doc.layers = layers;
        if self.doc.layers.is_empty() {
            let empty = self.layer_with("Layer 1", Vec::new());
            self.doc.layers.push(Arc::new(empty));
        }
        self.doc.guides = guides;
        Ok(())
    }

    /// A CMYK document when most process colours are CMYK (as a PDF opens), with the CMYK default
    /// swatches; then the file's named colours.
    fn finish(&mut self) {
        if self.cmyk > self.rgb {
            self.doc.color_mode = ColorMode::Cmyk;
            let (swatches, groups) = vectorcraft_color::default_swatches(ColorMode::Cmyk.model());
            (self.doc.swatches, self.doc.swatch_groups) = (swatches.into(), groups.into());
        }
        self.doc.swatches.extend(std::mem::take(&mut self.named));
    }

    fn layer_with(&mut self, name: &str, children: Vec<Arc<Node>>) -> Node {
        let id = self.doc.alloc_id();
        self.layer = self.layer.wrapping_add(1);
        let label = if name.trim().is_empty() { format!("Layer {}", self.layer) } else { name.to_string() };
        let mut n = Node::layer(id, &label, LayerColor::Preset(self.layer % 9));
        if let Some(c) = n.children_mut() {
            *c = children;
        }
        n
    }

    fn layer(&mut self, l: &cd::Layer, to_doc: Affine) -> Arc<Node> {
        let children = l.nodes.iter().filter_map(|n| self.node(n, to_doc)).map(Arc::new).collect();
        let mut n = self.layer_with(&l.name, children);
        n.visible = l.visible;
        n.locked = l.locked;
        if let NodeKind::Layer { printable, .. } = &mut n.kind {
            *printable = l.printable;
        }
        Arc::new(n)
    }

    fn blend(b: cd::Blend) -> BlendMode {
        match b {
            cd::Blend::Normal => BlendMode::Normal,
            cd::Blend::Multiply => BlendMode::Multiply,
            cd::Blend::Screen => BlendMode::Screen,
            cd::Blend::Overlay => BlendMode::Overlay,
        }
    }

    /// One CorelDRAW object (and what it holds) as VectorCraft art; `None` for one with nothing
    /// to show (an empty group, text whose characters could not be read).
    fn node(&mut self, n: &cd::Node, to_doc: Affine) -> Option<Node> {
        let mut out = match &n.kind {
            cd::Kind::Group { children } => {
                let kids: Vec<Arc<Node>> = children.iter().filter_map(|c| self.node(c, to_doc)).map(Arc::new).collect();
                if kids.is_empty() {
                    return None;
                }
                Node::new(self.doc.alloc_id(), NodeKind::Group { children: kids, clip: false })
            }
            cd::Kind::Shape { path, fill, outline, even_odd, outline_behind } => {
                let data = self.path(path, to_doc)?;
                let rule = if *even_odd { FillRule::EvenOdd } else { FillRule::NonZero };
                let mut p = Node::new(self.doc.alloc_id(), NodeKind::Path { path: data, rule, live: None, clipping: false, guide: false });
                p.appearance = self.appearance(fill, outline.as_ref(), *outline_behind, to_doc).into();
                p
            }
            cd::Kind::Text(t) => {
                let text = self.text(t, to_doc)?;
                Node::new(self.doc.alloc_id(), NodeKind::Text(Box::new(text)))
            }
            cd::Kind::Image(img) => self.image(img, to_doc)?,
            cd::Kind::PowerClip { frame, contents, contents_locked } => {
                return self.powerclip(n, frame, contents, *contents_locked, to_doc);
            }
        };
        self.decorate(&mut out, n, to_doc);
        Some(out)
    }

    /// Name, visibility, lock, opacity, blend mode and transparency ramp of `n` on `out`.
    fn decorate(&mut self, out: &mut Node, n: &cd::Node, to_doc: Affine) {
        out.name = n.name.clone().filter(|s| !s.trim().is_empty());
        out.visible = n.visible;
        out.locked = n.locked;
        out.opacity = unit(if n.opacity.is_finite() { n.opacity } else { 1.0 });
        out.blend = Self::blend(n.blend);
        if let Some(m) = &n.opacity_mask {
            let stored = n.bounds.map(|r| to_doc.transform_rect_bbox(Rect::new(r.x0, r.y0, r.x1, r.y1)));
            let reach = vectorcraft_geom::union_opt(out.visual_bounds(), stored).filter(|r| finite_rect(*r));
            match reach.and_then(|r| self.mask(m, r, to_doc)) {
                Some(mask) => out.extras_mut().mask = Some(Box::new(mask)),
                None => self.warn("transparency ramps on objects without bounds were left out"),
            }
        }
    }

    /// A transparency ramp as an opacity mask: a rectangle over everything `n` paints (`reach`),
    /// filled with a grey gradient placed like the ramp (white = opaque). Outside the ramp its end
    /// values extend, as the reader describes.
    fn mask(&mut self, m: &cd::OpacityMask, reach: Rect, to_doc: Affine) -> Option<OpacityMask> {
        let stops: Vec<GradientStop> = m
            .stops
            .iter()
            .map(|s| {
                let mut stop = GradientStop::new(unit(s.offset), Color::Gray { k: 1.0 - unit(s.opacity) });
                stop.midpoint = midpoint(s.midpoint);
                stop
            })
            .collect();
        let kind = self.kind(m.kind);
        let paint = self.placed(kind, stops, m.transform, to_doc)?;
        // Strokes and antialiasing reach a little past the geometric bounds.
        let pad = 1.0 + 0.02 * reach.width().max(reach.height());
        let rect = reach.inflate(pad, pad);
        let mut art = Node::new(
            self.doc.alloc_id(),
            NodeKind::Path { path: shapes::rectangle(rect), rule: FillRule::NonZero, live: None, clipping: false, guide: false },
        );
        art.appearance = Appearance { items: vec![AppearanceItem::Fill(FillLayer::new(paint))], ..Appearance::default() }.into();
        Some(OpacityMask::new(art, true))
    }

    /// A PowerClip as the user's own: a clip group whose first child is the frame (keeping its
    /// paint, marked as Place Inside Frame marks it) and whose other children are the contents.
    fn powerclip(&mut self, n: &cd::Node, frame: &cd::Node, contents: &[cd::Node], locked: bool, to_doc: Affine) -> Option<Node> {
        let mut kids: Vec<Arc<Node>> = contents.iter().filter_map(|c| self.node(c, to_doc)).map(Arc::new).collect();
        let mut frame_node = match self.node(frame, to_doc) {
            Some(f) if is_frame(&f) => f,
            built => {
                // A frame that can't clip. A bitmap's outline becomes the frame and its picture
                // the bottom content, which is how it draws; a frame that didn't come in at all
                // (text whose characters can't be read) clips by its stored bounds.
                let image = matches!(frame.kind, cd::Kind::Image(_));
                let outline = match &frame.kind {
                    cd::Kind::Image(img) => Some(img.clip.clone().unwrap_or_else(|| picture_outline(img))),
                    _ => n.bounds.map(|r| rect_path(Rect::new(r.x0, r.y0, r.x1, r.y1))),
                };
                if let Some(b) = built {
                    kids.insert(0, Arc::new(b));
                }
                let Some(data) = outline.and_then(|p| self.path(&p, to_doc)) else {
                    self.warn("PowerClips whose frame could not be imported came in as plain groups");
                    if kids.is_empty() {
                        return None;
                    }
                    let mut g = Node::new(self.doc.alloc_id(), NodeKind::Group { children: kids, clip: false });
                    self.decorate(&mut g, n, to_doc);
                    return Some(g);
                };
                self.warn(if image {
                    "bitmaps used as PowerClip frames: their outline became the frame and the picture its bottom content"
                } else {
                    "PowerClip frames that could not be imported clip by their stored bounds"
                });
                Node::new(self.doc.alloc_id(), NodeKind::Path { path: data, rule: FillRule::EvenOdd, live: None, clipping: false, guide: false })
            }
        };
        mark_frame(&mut frame_node);
        let mut children = vec![Arc::new(frame_node)];
        children.extend(kids);
        let mut g = Node::new(self.doc.alloc_id(), NodeKind::Group { children, clip: true });
        g.extras_mut().powerclip = Some(PowerClipSpec { contents_locked: locked });
        // The reader moves the container's name to the PowerClip and leaves its transparency on
        // the frame, which is what it applies to.
        self.decorate(&mut g, n, to_doc);
        Some(g)
    }

    /// A path in page space as document geometry; `None` when it has no subpaths.
    fn path(&self, p: &cd::Path, to_doc: Affine) -> Option<PathData> {
        if p.is_empty() {
            return None;
        }
        let mut bp = BezPath::new();
        for s in &p.subpaths {
            bp.move_to(point(s.start));
            for g in &s.segments {
                match g {
                    cd::Segment::Line(e) => bp.line_to(point(*e)),
                    cd::Segment::Cubic(a, b, e) => bp.curve_to(point(*a), point(*b), point(*e)),
                }
            }
            if s.closed {
                bp.close_path();
            }
        }
        Some(PathData::from_bezpath(&(to_doc * bp)))
    }

    /// A colour as VectorCraft stores it, counting process colours by model, and for a named one
    /// the global swatch it links to and its tint (the colour is then that tint of the swatch's).
    fn linked(&mut self, c: &cd::Color) -> (Color, Option<(String, f32)>) {
        match c {
            cd::Color::Rgb { r, g, b } => {
                self.rgb += 1;
                (Color::Rgb { r: unit(*r), g: unit(*g), b: unit(*b) }, None)
            }
            cd::Color::Cmyk { c, m, y, k } => {
                self.cmyk += 1;
                (Color::Cmyk { c: unit(*c), m: unit(*m), y: unit(*y), k: unit(*k) }, None)
            }
            // The reader's grey is a lightness; VectorCraft's is an ink amount.
            cd::Color::Gray { v } => (Color::Gray { k: 1.0 - unit(*v) }, None),
            cd::Color::Lab { l, a, b } => {
                let f = |v: f64, lo: f64, hi: f64| if v.is_finite() { v.clamp(lo, hi) as f32 } else { 0.0 };
                (Color::Lab { l: f(*l, 0.0, 100.0), a: f(*a, -128.0, 127.0), b: f(*b, -128.0, 127.0) }, None)
            }
            cd::Color::Named { name, spot, tint, base, .. } => {
                // A named colour's base is a process colour; one nested deeper isn't linked twice.
                let base = match base.as_ref() {
                    cd::Color::Named { .. } => self.color_flat(base),
                    other => self.linked(other).0,
                };
                let tint = unit(*tint);
                match self.swatch(name, base, *spot) {
                    Some(swatch) => (base.tinted(tint), Some((swatch, tint))),
                    None => (base.tinted(tint), None),
                }
            }
        }
    }

    /// A nested named colour's plain value (never deeper than the reader builds them).
    fn color_flat(&mut self, c: &cd::Color) -> Color {
        let mut c = c;
        let mut tint = 1.0f32;
        for _ in 0..8 {
            match c {
                cd::Color::Named { tint: t, base, .. } => {
                    tint *= unit(*t);
                    c = base;
                }
                other => return self.linked(other).0.tinted(tint),
            }
        }
        Color::BLACK
    }

    /// The global swatch named `name` holding `color`, made on first use; a name already taken by
    /// another colour (or a default swatch) gets a number. `None` when no name is free.
    fn swatch(&mut self, name: &str, color: Color, spot: bool) -> Option<String> {
        let paint = Paint::solid(color);
        let base = if name.trim().is_empty() { "CorelDRAW colour" } else { name.trim() };
        for i in 1..=MAX_NAME_TRIES {
            let candidate = if i == 1 { base.to_string() } else { format!("{base} {i}") };
            match self.named.iter().find(|s| s.name == candidate) {
                Some(s) if s.paint == paint && s.spot == spot => return Some(candidate),
                Some(_) => continue,
                None if self.doc.swatches.iter().any(|s| s.name == candidate) => continue,
                None => {
                    self.named.push(Swatch { name: candidate.clone(), paint, global: true, spot });
                    return Some(candidate);
                }
            }
        }
        self.warn("named colours whose name could not be given to a swatch came in unlinked");
        None
    }

    fn solid(&mut self, c: &cd::Color) -> Paint {
        match self.linked(c) {
            (color, Some((swatch, tint))) => Paint::Solid { color, swatch: Some(swatch), tint },
            (color, None) => Paint::solid(color),
        }
    }

    fn kind(&mut self, k: cd::GradientKind) -> GradientKind {
        match k {
            cd::GradientKind::Linear => GradientKind::Linear,
            cd::GradientKind::Radial => GradientKind::Radial,
            cd::GradientKind::Conical => {
                self.warn("conical fountain fills and transparencies import as radial gradients");
                GradientKind::Radial
            }
            cd::GradientKind::Square => {
                self.warn("square fountain fills and transparencies import as radial gradients");
                GradientKind::Radial
            }
        }
    }

    /// A gradient of `stops` placed by the reader's gradient → page map `m`: in gradient space a
    /// linear gradient runs from (0, 0) to (1, 0), a radial one is the unit circle; mapping that
    /// placement through `m` keeps skews and uneven scaling (a circle maps onto an ellipse).
    fn placed(&mut self, kind: GradientKind, stops: Vec<GradientStop>, m: cd::Affine, to_doc: Affine) -> Option<Paint> {
        if stops.is_empty() {
            return None;
        }
        let m = to_doc * affine(m);
        if !m.as_coeffs().iter().all(|v| v.is_finite()) {
            return None;
        }
        let mut geom = GradientGeom { start: Point::ZERO, end: Point::new(1.0, 0.0), aspect: 1.0, focal: None };
        geom.transform(m, kind);
        if ![geom.start.x, geom.start.y, geom.end.x, geom.end.y, geom.aspect].iter().all(|v| v.is_finite()) {
            return None;
        }
        geom.aspect = geom.aspect.clamp(0.001, 1000.0);
        let mut gp = GradientPaint::new(Gradient { kind, stops });
        gp.angle = geom.angle_deg();
        gp.geom = Some(geom);
        Some(Paint::Gradient(Box::new(gp)))
    }

    fn paint(&mut self, p: &cd::Paint, to_doc: Affine) -> Option<Paint> {
        match p {
            cd::Paint::None => None,
            cd::Paint::Solid(c) => Some(self.solid(c)),
            cd::Paint::Gradient(g) => {
                let kind = self.kind(g.kind);
                let stops = g
                    .stops
                    .iter()
                    .map(|s| {
                        let (color, link) = self.linked(&s.color);
                        let mut stop = GradientStop::new(unit(s.offset), color);
                        stop.set_color(color, link);
                        stop.opacity = unit(s.opacity);
                        stop.midpoint = midpoint(s.midpoint);
                        stop
                    })
                    .collect();
                self.placed(kind, stops, g.transform, to_doc)
            }
            // The reader names what it doesn't decode in its warnings; nothing is painted.
            cd::Paint::Unsupported(_) => None,
        }
    }

    fn stroke(&mut self, o: &cd::Outline, to_doc: Affine) -> Option<StrokeLayer> {
        if !(o.width.is_finite() && o.width > 0.0) {
            return None;
        }
        let paint = self.paint(&o.paint, to_doc)?;
        let mut l = StrokeLayer::new(paint, o.width);
        l.cap = cap(o.cap);
        l.join = join(o.join);
        l.miter_limit = if o.miter_limit.is_finite() { o.miter_limit.clamp(1.0, 500.0) } else { 4.0 };
        l.dash = o.dash.as_ref().filter(|d| !d.is_empty() && d.iter().all(|v| v.is_finite() && *v >= 0.0)).map(|pattern| Dash {
            pattern: pattern.clone(),
            offset: 0.0,
            align_corners: false,
        });
        Some(l)
    }

    /// A fill and an outline; an outline drawn behind the fill comes first.
    fn appearance(&mut self, fill: &cd::Paint, outline: Option<&cd::Outline>, behind: bool, to_doc: Affine) -> Appearance {
        let mut items = Vec::new();
        if let Some(paint) = self.paint(fill, to_doc) {
            items.push(AppearanceItem::Fill(FillLayer::new(paint)));
        }
        if let Some(stroke) = outline.and_then(|o| self.stroke(o, to_doc)) {
            let behind = behind || outline.is_some_and(|o| o.behind_fill);
            if behind {
                items.insert(0, AppearanceItem::Stroke(stroke));
            } else {
                items.push(AppearanceItem::Stroke(stroke));
            }
        }
        Appearance { items, ..Appearance::default() }
    }

    /// Artistic text as point type: text space is the reader's (origin at the first baseline's
    /// start, y down, upright glyphs), so its transform places the object; sizes already carry any
    /// uniform scale. Fonts resolve by PostScript name first, then by family and style.
    fn text(&mut self, t: &cd::Text, to_doc: Affine) -> Option<TextObject> {
        if t.runs.is_empty() {
            return None;
        }
        let xf = to_doc * affine(t.transform);
        if !xf.as_coeffs().iter().all(|v| v.is_finite()) || xf.determinant().abs() < 1e-12 {
            return None;
        }
        let mut obj = TextObject::point(Point::ZERO, "", CharStyle::default());
        obj.xf = xf;
        let fonts = vectorcraft_text::FontDb::global();
        let mut runs = Vec::with_capacity(t.runs.len());
        for r in &t.runs {
            let mut style = CharStyle::default();
            match r.postscript_name.as_deref().filter(|n| !n.is_empty()).and_then(|n| fonts.by_postscript_name(n)) {
                Some((family, face)) => {
                    style.font_family = family;
                    style.font_style = face;
                }
                None => {
                    if r.family.trim().is_empty() {
                        self.warn("text without a font name uses the default font");
                    } else {
                        style.font_family = r.family.clone();
                    }
                    style.font_style = super::super::affinity::font_style(i64::from(r.weight), r.italic);
                }
            }
            style.size = if r.size_pt.is_finite() && r.size_pt > 0.0 { r.size_pt.clamp(0.1, 100_000.0) } else { style.size };
            style.tracking = if r.tracking.is_finite() { (r.tracking * 1000.0).clamp(-1000.0, 10_000.0) } else { 0.0 };
            style.underline = r.underline;
            style.fill = self.paint(&r.fill, to_doc).unwrap_or(Paint::None);
            if let Some(o) = &r.outline
                && let Some(stroke) = self.stroke(o, to_doc)
            {
                style.stroke = stroke.paint;
                style.stroke_width = stroke.width;
                style.stroke_cap = stroke.cap;
                style.stroke_join = stroke.join;
                style.stroke_miter_limit = stroke.miter_limit;
                style.stroke_dash = stroke.dash;
            }
            runs.push(TextRun::new(r.text.clone(), style));
        }
        obj.runs = runs;
        obj.para.justify = match t.align {
            cd::Align::Left => Justify::Left,
            cd::Align::Center => Justify::Center,
            cd::Align::Right => Justify::Right,
            cd::Align::Justify => Justify::JustifyLeft,
        };
        Some(obj)
    }

    /// A bitmap object as an image (in a clip group by its crop outline when it is cropped).
    fn image(&mut self, img: &cd::Image, to_doc: Affine) -> Option<Node> {
        let pic = self.picture(img)?;
        // The pixel grid maps onto the object's own picture size, whatever the stored file's.
        let sx = f64::from(img.width) / f64::from(pic.width.max(1));
        let sy = f64::from(img.height) / f64::from(pic.height.max(1));
        let xf = to_doc * affine(img.transform) * Affine::scale_non_uniform(sx, sy);
        if !xf.as_coeffs().iter().all(|v| v.is_finite()) {
            return None;
        }
        let image = Node::new(
            self.doc.alloc_id(),
            NodeKind::Image(Box::new(ImageObject {
                key: pic.key,
                width: pic.width,
                height: pic.height,
                xf,
                link: None,
                placement: Default::default(),
            })),
        );
        let Some(clip) = img.clip.as_ref().and_then(|c| self.path(c, to_doc)) else { return Some(image) };
        let clip = Node::new(self.doc.alloc_id(), NodeKind::Path { path: clip, rule: FillRule::EvenOdd, live: None, clipping: true, guide: false });
        Some(Node::new(self.doc.alloc_id(), NodeKind::Group { children: vec![Arc::new(clip), Arc::new(image)], clip: true }))
    }

    /// The stored picture of a bitmap: its pixels as a PNG in `Document::images`, made once per
    /// picture (the reader shares one buffer between the objects that show it).
    fn picture(&mut self, img: &cd::Image) -> Option<Picture> {
        let key = Arc::as_ptr(&img.rgba) as usize;
        if let Some((_, made)) = self.pictures.get(&key) {
            return made.clone();
        }
        let made = encode(img).and_then(|png| super::super::load::raster_image(&png).ok()).map(|r| {
            self.doc.images.entry(r.key.clone()).or_insert(r.blob);
            Picture { key: r.key, width: r.width, height: r.height }
        });
        if made.is_none() {
            self.warn("bitmaps whose pixels could not be stored were left out");
        }
        self.pictures.insert(key, (Arc::clone(&img.rgba), made.clone()));
        made
    }
}

/// A picture's straight-alpha pixels as a PNG (RGB when every pixel is opaque).
fn encode(img: &cd::Image) -> Option<Vec<u8>> {
    let expected = u64::from(img.width).checked_mul(u64::from(img.height))?.checked_mul(4)?;
    if img.rgba.len() as u64 != expected || img.width == 0 || img.height == 0 {
        return None;
    }
    let mut png = Vec::new();
    let (pixels, _) = img.rgba.as_chunks::<4>();
    if pixels.iter().all(|&[.., a]| a == 255) {
        let rgb: Vec<u8> = pixels.iter().flat_map(|&[r, g, b, _]| [r, g, b]).collect();
        image::RgbImage::from_raw(img.width, img.height, rgb)?.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    } else {
        image::RgbaImage::from_raw(img.width, img.height, img.rgba.to_vec())?.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    }
    Some(png)
}

/// The rectangle a picture covers on the page.
fn picture_outline(img: &cd::Image) -> cd::Path {
    let (w, h) = (f64::from(img.width), f64::from(img.height));
    let m = img.transform;
    let corner = |x: f64, y: f64| m.apply(cd::Point::new(x, y));
    cd::Path {
        subpaths: vec![cd::SubPath {
            start: corner(0.0, 0.0),
            segments: vec![cd::Segment::Line(corner(w, 0.0)), cd::Segment::Line(corner(w, h)), cd::Segment::Line(corner(0.0, h))],
            closed: true,
        }],
    }
}

fn rect_path(r: Rect) -> cd::Path {
    let p = cd::Point::new;
    cd::Path {
        subpaths: vec![cd::SubPath {
            start: p(r.x0, r.y0),
            segments: vec![cd::Segment::Line(p(r.x1, r.y0)), cd::Segment::Line(p(r.x1, r.y1)), cd::Segment::Line(p(r.x0, r.y1))],
            closed: true,
        }],
    }
}

/// A stop's midpoint within VectorCraft's range (0.13..0.87; 0.5 when it isn't a number).
fn midpoint(m: f64) -> f32 {
    if m.is_finite() { m.clamp(0.13, 0.87) as f32 } else { 0.5 }
}

fn cap(c: cd::Cap) -> LineCap {
    match c {
        cd::Cap::Butt => LineCap::Butt,
        cd::Cap::Round => LineCap::Round,
        cd::Cap::Square => LineCap::Square,
    }
}

fn join(j: cd::Join) -> LineJoin {
    match j {
        cd::Join::Miter => LineJoin::Miter,
        cd::Join::Round => LineJoin::Round,
        cd::Join::Bevel => LineJoin::Bevel,
    }
}
