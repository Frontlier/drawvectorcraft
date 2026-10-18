//! The document structure: pages → layers → groups → objects, PowerClips and z-order.
//!
//! ```text
//! RIFF CDR?
//! ├─ vrsn, fver                 format version
//! ├─ LIST doc                   mcfg (page size), clpt (PowerClip contents), filt/otlt/stlt (paint), fntt (fonts)
//! ├─ LIST bmpt                  bmp records (bitmaps)
//! └─ LIST page …                the master page (flgs bit 0x10000) and the document pages, in order
//!    └─ LIST gobj / LIST layr   flgs, spid, LIST lgob (loda: kind, name, GUID, colour), drawables
//!       drawable := LIST obj | LIST grp | LIST lnkg
//!       obj      := [spnd] flgs spid bbox obbx [usdn] LIST lgob(loda [ftil] [ttil] LIST trfl(trfd)) [txsm txtj]
//! ```
//!
//! Within every container the first child is the front-most, so children are reversed. An
//! object is placed by its own `trfd` only (group transforms are already baked into their
//! children). A PowerClip container is an object with loda arg 8005 naming the `spnd` key of a
//! `doc/clpt` group, whose children are the clipped contents. A record that fails to decode is
//! skipped with a warning; a document where most objects fail is refused as unsupported.

use std::collections::{HashMap, HashSet};

use crate::container::{Container, Limits};
use crate::geometry::{self, Loda, arg, kind};
use crate::model::{Affine, Guide, Kind, Layer, Node, POINTS_PER_UNIT, Page, Paint, Path, Point, Rect, Transparency, Warnings};
use crate::{Document, Error, VERIFIED_VERSION, bitmap, paint, text};

/// Deepest nesting of groups and PowerClips followed; deeper content is reported, not imported.
pub(crate) const MAX_TREE_DEPTH: usize = 128;
/// Objects visited over the whole document, PowerClip contents and repeated master layers included.
pub(crate) const MAX_NODES: usize = 500_000;
/// Path points over the whole document.
pub(crate) const MAX_TOTAL_POINTS: usize = 8 << 20;
const MAX_PAGES: usize = 10_000;
const MAX_LAYERS: usize = 10_000;
const MAX_CLIP_GROUPS: usize = 100_000;
const MAX_GUIDES: usize = 10_000;
/// CorelDRAW's drawing area is ±900 inches around the page centre.
const DRAWING_LIMIT: i32 = 228_600_000;
/// Largest page side accepted from `mcfg` (twice the drawing area).
const MAX_PAGE_SIDE: i32 = 457_200_000;
/// Farthest a placed point may lie from the page origin, in points (about 350 km; CorelDRAW's
/// own drawing area reaches about 65 000 pt). Objects placed beyond are skipped.
const MAX_COORDINATE: f64 = 1e9;
/// A4 portrait, used when the page size can't be read.
const A4: (i32, i32) = (2_100_000, 2_970_000);

/// `flgs` bits 24–31: the node type code.
const TYPE_OBJECT: u32 = 0x08;
const TYPE_GROUP: u32 = 0x10;
const TYPE_LINK_GROUP: u32 = 0x18;
const FLAG_MASTER: u32 = 0x0001_0000;
const FLAG_POWERCLIP: u32 = 0x0010_0000;

/// Read a CorelDRAW document natively: container, chunk tree and model.
///
/// A record that fails to decode is skipped with a warning. Older layouts, and split documents
/// with no page, no art layer or more objects undecodable than decoded, give
/// [`Error::Unsupported`] so a caller can fall back to another importer or to [`crate::preview`];
/// a damaged container gives [`Error::Malformed`] and an exhausted budget [`Error::Limit`].
pub fn read(bytes: &[u8], limits: Limits) -> Result<Document, Error> {
    let c = Container::open(bytes, limits)?;
    Reader::new(&c, limits).document()
}

/// Why one object was not imported.
enum Fail {
    /// Skip the object with a warning.
    Skip(&'static str),
    /// Abort the import (a safety limit).
    Fatal(Error),
}

impl From<&'static str> for Fail {
    fn from(s: &'static str) -> Self {
        Fail::Skip(s)
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        match e {
            Error::Limit(_) => Fail::Fatal(e),
            Error::Malformed(s) | Error::Unsupported(s) => Fail::Skip(s),
        }
    }
}

/// A `layr` list as read from its records.
#[derive(Debug, Clone)]
struct LayerInfo {
    chunk: usize,
    kind: Option<u32>,
    master: bool,
    name: Option<String>,
    guid: Option<[u8; 16]>,
    color: Option<crate::Color>,
    has_drawables: bool,
}

struct Reader<'c, 'a> {
    c: &'c Container<'a>,
    w: Warnings,
    nodes: usize,
    points: usize,
    /// Objects met and objects that could not be decoded (the robustness gate).
    attempted: usize,
    failed: usize,
    clip_groups: HashMap<u32, usize>,
    clip_stack: Vec<u32>,
    clip_used: HashSet<u32>,
    /// World units → page space.
    page: Affine,
    paint: paint::Tables,
    text: text::Tables,
    bitmaps: bitmap::Store,
    pixels_left: u64,
}

const OUT_OF_RANGE: &str = "an object placed beyond any usable range";

/// A placement whose coefficients (scale, rotation and origin) are finite and within range.
fn usable(m: &Affine) -> bool {
    m.is_finite() && m.0.iter().all(|v| v.abs() <= MAX_COORDINATE)
}

/// A stored `bbox` (xMin, yMax, xMax, yMin) as a rectangle in world units.
fn world_rect(x0: i32, y1: i32, x1: i32, y0: i32) -> Rect {
    Rect::from_points(Point::new(f64::from(x0), f64::from(y1)), Point::new(f64::from(x1), f64::from(y0)))
}

fn is_drawable(id: &[u8; 4]) -> bool {
    matches!(id, b"obj " | b"grp " | b"lnkg")
}

/// A chunk id for a warning: printable ASCII as is, anything else in hex.
fn printable(id: &[u8; 4]) -> String {
    if id.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        id.iter().map(|b| char::from(*b)).collect::<String>().trim_end().to_string()
    } else {
        id.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl<'c, 'a> Reader<'c, 'a> {
    fn new(c: &'c Container<'a>, limits: Limits) -> Self {
        Self {
            c,
            w: Warnings::default(),
            nodes: 0,
            points: 0,
            attempted: 0,
            failed: 0,
            clip_groups: HashMap::new(),
            clip_stack: Vec::new(),
            clip_used: HashSet::new(),
            page: Affine::IDENTITY,
            paint: paint::Tables::default(),
            text: text::Tables::default(),
            bitmaps: bitmap::Store::default(),
            pixels_left: limits.max_pixels,
        }
    }

    fn document(mut self) -> Result<Document, Error> {
        let c = self.c;
        let t = &c.tree;
        let root = t.root();
        let version = c.version().unwrap_or_else(|| {
            self.w.warn("no format version record");
            0
        });
        if version != 0 && version != VERIFIED_VERSION {
            self.w.warn(format!("format version {version} has not been verified (the reader was checked on version {VERIFIED_VERSION})"));
        }
        let doc = t.child_list(root, b"doc ").ok_or(Error::Malformed("no document list"))?;
        let (width, height) = self.page_size(doc);
        let k = POINTS_PER_UNIT;
        // x_pt = (x + W/2)·k, y_pt = (H/2 − y)·k: the page's top-left corner becomes the origin, y down.
        self.page = Affine([k, 0.0, 0.0, -k, f64::from(width) / 2.0 * k, f64::from(height) / 2.0 * k]);
        self.clip_groups = self.clip_table(doc)?;
        self.paint = paint::Tables::load(c, doc, &mut self.w);
        self.text = text::Tables::load(c, doc, &mut self.w);
        self.bitmaps = bitmap::Store::load(c, root, &mut self.w);

        let pages: Vec<usize> = t.children(root).filter(|i| t.get(*i).is_some_and(|ch| ch.list && &ch.id == b"page")).collect();
        if pages.len() > MAX_PAGES {
            return Err(Error::Limit("too many pages"));
        }
        let mut master = None;
        let mut document_pages = Vec::new();
        for p in pages {
            if master.is_none() && self.flags(p).is_some_and(|f| f & FLAG_MASTER != 0) {
                master = Some(p);
            } else {
                if self.flags(p).is_some_and(|f| f & FLAG_MASTER != 0) {
                    self.w.warn("several master pages (the first is used; the others are read as pages)");
                }
                document_pages.push(p);
            }
        }
        if document_pages.is_empty() {
            return Err(Error::Unsupported("the document has no pages"));
        }
        let master_layers = match master {
            Some(m) => self.layer_infos(m)?,
            None => Vec::new(),
        };
        let mut master_guides = Vec::new();
        for l in master_layers.iter().filter(|l| l.kind == Some(kind::GUIDES_LAYER)) {
            master_guides.extend(self.guides(l.chunk)?);
        }
        let mut out = Vec::with_capacity(document_pages.len());
        let mut art_layers = 0usize;
        for (n, p) in document_pages.into_iter().enumerate() {
            let page = self.page_of(n, p, &master_layers, &master_guides, width, height)?;
            art_layers += page.layers.len();
            out.push(page);
        }
        if art_layers == 0 {
            return Err(Error::Unsupported("no art layer found"));
        }
        if self.failed > 0 && self.failed * 2 > self.attempted {
            return Err(Error::Unsupported("most objects could not be decoded"));
        }
        Ok(Document { version, pages: out, warnings: self.w.finish() })
    }

    /// `doc/mcfg`: i32 page width @12 and height @16 in world units. The page rectangle is
    /// centred on the origin.
    fn page_size(&mut self, doc: usize) -> (i32, i32) {
        let c = self.c;
        let record = c.tree.child_leaf(doc, b"mcfg").and_then(|i| c.payload(i).ok());
        // u32 @4 is the body length (the record minus its 8-byte header).
        let body = record.and_then(|b| geometry::u32_at(b, 4)).and_then(|l| usize::try_from(l).ok()).and_then(|l| l.checked_add(8));
        if record.is_some_and(|b| body != Some(b.len())) {
            self.w.warn("a page setup record of an unexpected length");
        }
        let size = record.and_then(|b| Some((geometry::i32_at(b, 12)?, geometry::i32_at(b, 16)?)));
        match size {
            Some((w, h)) if (1..=MAX_PAGE_SIDE).contains(&w) && (1..=MAX_PAGE_SIDE).contains(&h) => (w, h),
            _ => {
                self.w.warn("no page size (A4 assumed)");
                A4
            }
        }
    }

    /// `doc/clpt`: PowerClip contents groups by their `spnd` key.
    fn clip_table(&mut self, doc: usize) -> Result<HashMap<u32, usize>, Error> {
        let c = self.c;
        let mut map = HashMap::new();
        let Some(clpt) = c.tree.child_list(doc, b"clpt") else { return Ok(map) };
        for g in c.tree.children(clpt) {
            if !c.tree.get(g).is_some_and(|ch| ch.list && &ch.id == b"grp ") {
                continue;
            }
            if map.len() >= MAX_CLIP_GROUPS {
                return Err(Error::Limit("too many PowerClips"));
            }
            let key = c.tree.child_leaf(g, b"spnd").and_then(|i| c.payload(i).ok()).filter(|b| b.len() == 4).and_then(|b| geometry::u32_at(b, 0));
            match key {
                Some(k) if !map.contains_key(&k) => {
                    map.insert(k, g);
                }
                Some(_) => self.w.warn("PowerClip contents with a repeated key (the first is used)"),
                None => self.w.warn("PowerClip contents without a key (not imported)"),
            }
        }
        Ok(map)
    }

    fn flags(&self, list: usize) -> Option<u32> {
        let c = self.c;
        c.tree.child_leaf(list, b"flgs").and_then(|i| c.payload(i).ok()).filter(|b| b.len() == 4).and_then(|b| geometry::u32_at(b, 0))
    }

    fn check_flags(&mut self, list: usize, type_code: u32) -> Option<u32> {
        let flags = self.flags(list);
        match flags {
            None => self.w.warn("objects without flags"),
            Some(f) if f >> 24 != type_code => self.w.warn("objects whose flags disagree with their record type"),
            Some(_) => {}
        }
        flags
    }

    /// The `loda` of a list's `lgob`, decoded.
    fn loda_of(&self, list: usize) -> Result<Loda<'c>, Fail> {
        let c = self.c;
        let lgob = c.tree.child_list(list, b"lgob").ok_or("an object without attributes")?;
        let loda = c.tree.child_leaf(lgob, b"loda").ok_or("an object without an object record")?;
        Ok(Loda::parse(c.payload(loda)?)?)
    }

    fn layer_infos(&mut self, page: usize) -> Result<Vec<LayerInfo>, Error> {
        let c = self.c;
        let Some(gobj) = c.tree.child_list(page, b"gobj") else {
            self.w.warn("a page without layers");
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for l in c.tree.children(gobj) {
            let Some(ch) = c.tree.get(l) else { continue };
            if !ch.list || &ch.id != b"layr" {
                if ch.list && is_drawable(&ch.id) {
                    self.w.warn("objects outside any layer (not imported)");
                }
                continue;
            }
            if out.len() >= MAX_LAYERS {
                return Err(Error::Limit("too many layers"));
            }
            let master = self.flags(l).is_some_and(|f| f & FLAG_MASTER != 0);
            let loda = match self.loda_of(l) {
                Ok(loda) => Some(loda),
                Err(Fail::Fatal(e)) => return Err(e),
                Err(Fail::Skip(_)) => {
                    self.w.warn("a layer record that could not be decoded");
                    None
                }
            };
            let color = loda.as_ref().and_then(|d| d.arg(arg::LAYER_COLOR)).and_then(|b| paint::color_block(b).ok());
            let guid = loda.as_ref().and_then(|d| d.arg(arg::LAYER_GUID)).and_then(|b| <[u8; 16]>::try_from(b).ok());
            let has_drawables = c.tree.children(l).any(|i| c.tree.get(i).is_some_and(|ch| ch.list && is_drawable(&ch.id)));
            out.push(LayerInfo {
                chunk: l,
                kind: loda.as_ref().map(|d| d.obj_type),
                master,
                name: loda.as_ref().and_then(Loda::name),
                guid,
                color,
                has_drawables,
            });
        }
        Ok(out)
    }

    fn page_of(
        &mut self,
        n: usize,
        page: usize,
        master_layers: &[LayerInfo],
        master_guides: &[Guide],
        width: i32,
        height: i32,
    ) -> Result<Page, Error> {
        let c = self.c;
        let infos = self.layer_infos(page)?;
        let mut guides = master_guides.to_vec();
        let mut used = HashSet::new();
        // File order: the first layer is the front-most.
        let mut layers = Vec::new();
        self.clip_used.clear();
        for info in &infos {
            let mirrored =
                if info.master && !info.has_drawables { info.guid.and_then(|g| master_layers.iter().position(|m| m.guid == Some(g))) } else { None };
            let target = match mirrored {
                Some(i) => {
                    used.insert(i);
                    master_layers.get(i).unwrap_or(info)
                }
                None => info,
            };
            let from_master = mirrored.is_some();
            match target.kind {
                Some(kind::GROUP_OR_LAYER) => {
                    if from_master {
                        self.w.warn("master page layers (repeated on every page)");
                    }
                    layers.push(self.layer(target, from_master, true, layers.len())?);
                }
                Some(kind::GUIDES_LAYER) => {
                    if !from_master {
                        let local = self.guides(target.chunk)?;
                        guides.extend(local);
                    }
                }
                Some(kind::GRID_LAYER) => {
                    if target.has_drawables {
                        self.w.warn("objects on the grid layer (not imported)");
                    }
                }
                Some(kind::DESKTOP_LAYER) if target.has_drawables => {
                    self.w.warn("the desktop layer (imported as a layer)");
                    layers.push(self.layer(target, from_master, true, layers.len())?);
                }
                Some(kind::DESKTOP_LAYER) => {}
                _ if target.has_drawables => {
                    self.w.warn("a layer of an unknown kind (imported hidden)");
                    layers.push(self.layer(target, from_master, false, layers.len())?);
                }
                _ => {}
            }
        }
        // Master layers with content that this page does not mirror: shown in front of the page's own.
        let mut front = Vec::new();
        for (i, m) in master_layers.iter().enumerate() {
            if used.contains(&i) {
                continue;
            }
            let art =
                m.kind == Some(kind::GROUP_OR_LAYER) || (m.kind != Some(kind::GUIDES_LAYER) && m.kind != Some(kind::GRID_LAYER) && m.has_drawables);
            if art && m.has_drawables {
                self.w.warn("master page layers (repeated on every page)");
                let visible = matches!(m.kind, Some(kind::GROUP_OR_LAYER | kind::DESKTOP_LAYER));
                front.push(self.layer(m, true, visible, front.len())?);
            }
        }
        front.append(&mut layers);
        let mut layers = front;
        layers.reverse();
        if guides.len() > MAX_GUIDES {
            guides.truncate(MAX_GUIDES);
            self.w.warn("too many guidelines (the rest are not imported)");
        }
        let bounds = c
            .tree
            .child_leaf(page, b"bbox")
            .and_then(|i| c.payload(i).ok())
            .and_then(|b| geometry::bbox(b).ok())
            .map(|[x0, y1, x1, y0]| self.page_rect(x0, y1, x1, y0));
        let k = POINTS_PER_UNIT;
        Ok(Page { name: format!("Page {}", n + 1), width: f64::from(width) * k, height: f64::from(height) * k, layers, guides, bounds })
    }

    fn page_rect(&self, x0: i32, y1: i32, x1: i32, y0: i32) -> Rect {
        let a = self.page.apply(Point::new(f64::from(x0), f64::from(y1)));
        let b = self.page.apply(Point::new(f64::from(x1), f64::from(y0)));
        Rect::from_points(a, b)
    }

    fn layer(&mut self, info: &LayerInfo, master: bool, visible: bool, position: usize) -> Result<Layer, Error> {
        let nodes = self.children(info.chunk, 0)?;
        Ok(Layer {
            name: info.name.clone().unwrap_or_else(|| format!("Layer {}", position + 1)),
            visible,
            locked: false,
            printable: true,
            master,
            color: info.color.clone(),
            nodes,
        })
    }

    /// Guidelines of a guides layer, in page space.
    fn guides(&mut self, layer: usize) -> Result<Vec<Guide>, Error> {
        let c = self.c;
        let mut out = Vec::new();
        for o in c.tree.children(layer) {
            let Some(ch) = c.tree.get(o) else { continue };
            if !ch.list || !is_drawable(&ch.id) {
                continue;
            }
            let loda = match (&ch.id == b"obj ").then(|| self.loda_of(o)) {
                Some(Ok(l)) => l,
                Some(Err(Fail::Fatal(e))) => return Err(e),
                Some(Err(Fail::Skip(_))) => {
                    self.w.warn("a guideline that could not be decoded");
                    continue;
                }
                None => {
                    self.w.warn("objects on a guides layer (not imported)");
                    continue;
                }
            };
            if loda.obj_type != kind::GUIDELINE {
                self.w.warn("objects on a guides layer (not imported)");
                continue;
            }
            let Some(Ok((a, b))) = loda.arg(arg::GUIDELINE).map(geometry::guideline) else {
                self.w.warn("a guideline that could not be decoded");
                continue;
            };
            let world = self.transform_of(o).ok().map_or(Affine::IDENTITY, |(m, _)| m);
            let m = world.then(self.page);
            if out.len() < MAX_GUIDES {
                out.push(Guide { a: m.apply(a), b: m.apply(b) });
            }
        }
        Ok(out)
    }

    /// The object's own `lgob/trfl/trfd` (world = trfd · local).
    fn transform_of(&self, list: usize) -> Result<(Affine, usize), Fail> {
        let c = self.c;
        let lgob = c.tree.child_list(list, b"lgob").ok_or("an object without attributes")?;
        let trfd = c.tree.child_list(lgob, b"trfl").and_then(|l| c.tree.child_leaf(l, b"trfd")).ok_or("an object without a transform")?;
        Ok(geometry::transform(c.payload(trfd)?)?)
    }

    /// The drawables of a layer or group, bottom to top.
    fn children(&mut self, list: usize, depth: usize) -> Result<Vec<Node>, Error> {
        let c = self.c;
        let mut out = Vec::new();
        for i in c.tree.children(list) {
            let Some(ch) = c.tree.get(i) else { continue };
            match (ch.list, &ch.id) {
                (true, id) if is_drawable(id) => {
                    if let Some(n) = self.drawable(i, depth + 1)? {
                        out.push(n);
                    }
                }
                (true, b"lgob") => {}
                (false, b"flgs" | b"spid" | b"spnd" | b"usdn" | b"bbox" | b"obbx" | b"txsm" | b"txtj") => {}
                (_, id) => self.w.warn(format!("unknown records '{}' (ignored)", printable(id))),
            }
        }
        out.reverse();
        Ok(out)
    }

    fn drawable(&mut self, list: usize, depth: usize) -> Result<Option<Node>, Error> {
        if depth > MAX_TREE_DEPTH {
            self.w.warn("objects nested deeper than 128 levels (not imported)");
            return Ok(None);
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(Error::Limit("too many objects"));
        }
        let is_object = self.c.tree.get(list).is_some_and(|ch| &ch.id == b"obj ");
        if !is_object {
            return self.group(list, depth);
        }
        self.attempted += 1;
        match self.object(list, depth) {
            Ok(n) => Ok(n),
            Err(Fail::Fatal(e)) => Err(e),
            Err(Fail::Skip(reason)) => {
                self.failed += 1;
                self.w.warn(format!("objects that could not be read: {reason}"));
                Ok(None)
            }
        }
    }

    fn group(&mut self, list: usize, depth: usize) -> Result<Option<Node>, Error> {
        let link = self.c.tree.get(list).is_some_and(|ch| &ch.id == b"lnkg");
        self.check_flags(list, if link { TYPE_LINK_GROUP } else { TYPE_GROUP });
        let name = match self.loda_of(list) {
            Ok(l) => l.name(),
            Err(Fail::Fatal(e)) => return Err(e),
            Err(Fail::Skip(_)) => None,
        };
        let children = self.children(list, depth)?;
        // Groups left empty held only non-drawing objects.
        if children.is_empty() {
            return Ok(None);
        }
        let mut node = Node::new(Kind::Group { children });
        node.name = name;
        node.bounds = self.bbox_of(list).map(|[x0, y1, x1, y0]| self.page_rect(x0, y1, x1, y0));
        Ok(Some(node))
    }

    fn add_points(&mut self, n: usize) -> Result<(), Fail> {
        self.points = self.points.saturating_add(n);
        if self.points > MAX_TOTAL_POINTS {
            return Err(Fail::Fatal(Error::Limit("too many path points")));
        }
        Ok(())
    }

    fn object(&mut self, list: usize, depth: usize) -> Result<Option<Node>, Fail> {
        let c = self.c;
        let flags = self.check_flags(list, TYPE_OBJECT);
        let loda = self.loda_of(list)?;
        match loda.obj_type {
            kind::PLACEHOLDER => return Ok(None),
            kind::GUIDELINE => {
                self.w.warn("guidelines among the art (not imported)");
                return Ok(None);
            }
            _ => {}
        }
        let (world, parts) = self.transform_of(list)?;
        if parts > 1 {
            self.w.warn("transforms made of several parts (composed in order, unverified)");
        }
        let stored = self.bbox_of(list);
        if stored.is_some_and(|b| b.iter().any(|v| v.unsigned_abs() > DRAWING_LIMIT.unsigned_abs())) {
            self.w.warn("objects beyond CorelDRAW's drawing area (kept)");
        }
        let to_page = world.then(self.page);
        let lgob = c.tree.child_list(list, b"lgob");
        let tile = |id: &[u8; 4]| lgob.and_then(|l| c.tree.child_leaf(l, id)).and_then(|i| c.payload(i).ok());
        let mut target =
            paint::Target { loda: &loda, fill_tile: tile(b"ftil"), transparency_tile: tile(b"ttil"), local_bounds: None, world, to_page };
        let shape_payload = loda.arg(arg::GEOMETRY);
        let mut node = match loda.obj_type {
            kind::RECTANGLE => {
                let (path, unverified) = geometry::rectangle(shape_payload.ok_or("a rectangle without geometry")?)?;
                if unverified {
                    self.w.warn("scalloped or chamfered rectangle corners (unverified)");
                }
                self.shape(path, &mut target)?
            }
            kind::ELLIPSE => {
                let (path, partial) = geometry::ellipse(shape_payload.ok_or("an ellipse without geometry")?)?;
                if partial {
                    self.w.warn("elliptical arcs and pies (unverified)");
                }
                self.shape(path, &mut target)?
            }
            kind::CURVE => self.shape(geometry::curve(shape_payload.ok_or("a curve without geometry")?, geometry::MAX_PATH_POINTS)?, &mut target)?,
            kind::MESH => self.shape(geometry::curve(shape_payload.ok_or("a mesh without geometry")?, geometry::MAX_PATH_POINTS)?, &mut target)?,
            kind::TEXT => {
                let record = geometry::text(shape_payload.ok_or("a text without its record")?)?;
                if record.text_kind != 0 {
                    self.w.warn("paragraph text or other text kinds (unverified)");
                }
                // Text space (points, y down) → local units (y up), then the object's placement.
                let k = POINTS_PER_UNIT;
                let transform = Affine([1.0 / k, 0.0, 0.0, -1.0 / k, 0.0, 0.0]).then(to_page);
                if !usable(&transform) {
                    return Err(Fail::Skip(OUT_OF_RANGE));
                }
                // Text has no geometry of its own: a fountain fill is placed on the stored ink box.
                target.local_bounds = world.inverse().and_then(|inv| stored.map(|[x0, y1, x1, y0]| world_rect(x0, y1, x1, y0).transformed(&inv)));
                Node::new(Kind::Text(text::decode(&self.text, &self.paint, c, list, &target, transform, &mut self.w)))
            }
            kind::BITMAP => self.bitmap(shape_payload.ok_or("a bitmap without its record")?, &mut target)?,
            other => match shape_payload.and_then(|g| geometry::curve(g, geometry::MAX_PATH_POINTS).ok()) {
                Some(path) => {
                    self.w.warn(format!("objects of unknown type {other} (drawn from their stored outline, unverified)"));
                    self.shape(path, &mut target)?
                }
                None => {
                    self.w.warn(format!("objects of unknown type {other} (not imported)"));
                    return Ok(None);
                }
            },
        };
        node.name = loda.name();
        node.bounds = stored.map(|[x0, y1, x1, y0]| self.page_rect(x0, y1, x1, y0));
        let (transparency, blend) = self.paint.transparency(&target, &mut self.w);
        node.blend = blend;
        match transparency {
            Transparency::None | Transparency::Unsupported(_) => {}
            Transparency::Uniform(o) => node.opacity = if o.is_finite() { o.clamp(0.0, 1.0) } else { 1.0 },
            Transparency::Gradient(mask) => node.opacity_mask = Some(mask),
        }
        let clip_flag = flags.is_some_and(|f| f & FLAG_POWERCLIP != 0);
        match loda.arg(arg::POWERCLIP) {
            Some(record) => {
                if flags.is_some() && !clip_flag {
                    self.w.warn("PowerClip flags that disagree with the object record");
                }
                Ok(Some(self.powerclip(node, record, depth)?))
            }
            None => {
                if clip_flag {
                    self.w.warn("a PowerClip without its contents record (drawn without contents)");
                }
                Ok(Some(node))
            }
        }
    }

    /// A shape node from a path in local units.
    fn shape(&mut self, path: Path, target: &mut paint::Target<'_, '_>) -> Result<Node, Fail> {
        self.add_points(path.point_count())?;
        target.local_bounds = path.bounds();
        let fill = self.paint.fill(target, &mut self.w);
        let outline = self.paint.outline(target, &mut self.w);
        let outline_behind = outline.as_ref().is_some_and(|o| o.behind_fill);
        let path = path.transformed(&target.to_page);
        if !path.within(MAX_COORDINATE) {
            return Err(Fail::Skip(OUT_OF_RANGE));
        }
        Ok(Node::new(Kind::Shape { path, fill, outline, even_odd: paint::even_odd(target.loda), outline_behind }))
    }

    fn bitmap(&mut self, payload: &[u8], target: &mut paint::Target<'_, '_>) -> Result<Node, Fail> {
        let record = geometry::bitmap(payload, geometry::MAX_PATH_POINTS)?;
        self.add_points(record.clip.point_count())?;
        let to_page = target.to_page;
        let clip = record.clip.transformed(&to_page);
        if !clip.within(MAX_COORDINATE) {
            return Err(Fail::Skip(OUT_OF_RANGE));
        }
        // A transparency ramp is placed on the crop outline, like a fill on a shape.
        target.local_bounds = record.clip.bounds();
        // The fill of a bitmap object is not drawn (the picture is); an outline would be.
        if self.paint.outline(target, &mut self.w).is_some() {
            self.w.warn("outlines on bitmaps (not imported)");
        }
        if let Some(pixel) = record.pixel_to_local().map(|m| m.then(to_page)).filter(usable) {
            let crop = record.cropped.then(|| clip.clone());
            let c = self.c;
            if let Some(image) = bitmap::decode(&mut self.bitmaps, c, &record, pixel, crop, &mut self.pixels_left, &mut self.w) {
                return Ok(Node::new(Kind::Image(image)));
            }
        } else {
            self.w.warn("bitmaps with an empty frame");
        }
        // The picture could not be decoded: its crop outline stands in.
        Ok(Node::new(Kind::Shape { path: clip, fill: Paint::Unsupported("bitmap"), outline: None, even_odd: false, outline_behind: false }))
    }

    /// Wrap a container object around the contents of its `clpt` group.
    fn powerclip(&mut self, frame: Node, record: &[u8], depth: usize) -> Result<Node, Fail> {
        let key = (record.len() == 16).then(|| geometry::u32_at(record, 0)).flatten();
        let Some(key) = key else {
            self.w.warn("a PowerClip with a damaged contents record (drawn without contents)");
            return Ok(frame);
        };
        let Some(&group) = self.clip_groups.get(&key) else {
            self.w.warn("a PowerClip whose contents are missing (drawn without contents)");
            return Ok(frame);
        };
        if self.clip_stack.contains(&key) {
            self.w.warn("a PowerClip that contains itself (drawn without contents)");
            return Ok(frame);
        }
        if !self.clip_used.insert(key) {
            self.w.warn("PowerClip contents shared by several frames (repeated in each)");
        }
        self.clip_stack.push(key);
        let contents = self.children(group, depth);
        self.clip_stack.pop();
        let contents = contents.map_err(Fail::Fatal)?;
        let mut frame = frame;
        let name = frame.name.take();
        let bounds = frame.bounds;
        // Where CorelDRAW keeps "lock contents to PowerClip" is unknown; locked is its default.
        let mut node = Node::new(Kind::PowerClip { frame: Box::new(frame), contents, contents_locked: true });
        node.name = name;
        node.bounds = bounds;
        Ok(node)
    }

    /// The stored `bbox` of a drawable: xMin, yMax, xMax, yMin in world units.
    fn bbox_of(&self, list: usize) -> Option<[i32; 4]> {
        let c = self.c;
        c.tree.child_leaf(list, b"bbox").and_then(|i| c.payload(i).ok()).and_then(|b| geometry::bbox(b).ok())
    }
}
