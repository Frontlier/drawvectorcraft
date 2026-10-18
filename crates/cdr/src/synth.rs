//! Synthetic CorelDRAW-shaped files for tests and fuzz seeds (cargo feature `synth`).
//!
//! These builders write the "split" layout the reader accepts (a ZIP holding `content/root.dat`,
//! `content/dataFileList.dat` and `content/data/*.dat`, every leaf chunk a 16-byte reference), so
//! tests can describe documents in a few lines. They are **not** a CorelDRAW exporter: nothing
//! written here has been opened in CorelDRAW, every record carries only the fields the reader
//! looks at, and no byte comes from a real CorelDRAW file.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;

/// How a ZIP entry is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Stored,
    Deflate,
}

/// A ZIP archive with UTF-8 names, local headers that carry the sizes, and no extra fields.
pub fn zip(entries: &[(&str, &[u8], Method)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, method) in entries {
        let (code, stored) = match method {
            Method::Stored => (0u16, data.to_vec()),
            Method::Deflate => {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(data).unwrap();
                (8, e.finish().unwrap())
            }
        };
        let crc = crc32fast::hash(data);
        let offset = out.len() as u32;
        let mut header = Vec::new();
        header.extend(20u16.to_le_bytes()); // version needed
        header.extend(0x0800u16.to_le_bytes()); // UTF-8 names
        header.extend(code.to_le_bytes());
        header.extend(0u16.to_le_bytes()); // time
        header.extend(0x21u16.to_le_bytes()); // date: 1980-01-01
        header.extend(crc.to_le_bytes());
        header.extend((stored.len() as u32).to_le_bytes());
        header.extend((data.len() as u32).to_le_bytes());
        header.extend((name.len() as u16).to_le_bytes());
        header.extend(0u16.to_le_bytes()); // extra length
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(&header);
        out.extend(name.as_bytes());
        out.extend(&stored);
        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes()); // version made by
        central.extend(&header);
        central.extend(0u16.to_le_bytes()); // comment length
        central.extend(0u16.to_le_bytes()); // disk
        central.extend(0u16.to_le_bytes()); // internal attributes
        central.extend(0u32.to_le_bytes()); // external attributes
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let directory = out.len() as u32;
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(directory.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

/// A chunk of a RIFF tree: a leaf with its payload, or a list with its type and children.
#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Leaf([u8; 4], Vec<u8>),
    List([u8; 4], Vec<Chunk>),
}

pub fn leaf(id: &[u8; 4], payload: impl Into<Vec<u8>>) -> Chunk {
    Chunk::Leaf(*id, payload.into())
}

pub fn list(kind: &[u8; 4], children: Vec<Chunk>) -> Chunk {
    Chunk::List(*kind, children)
}

fn frame(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend(id);
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

/// A RIFF stream whose leaves hold their payloads inline (the layout of older files, which the
/// reader recognises but refuses).
pub fn riff_inline(form: &[u8; 4], children: &[Chunk]) -> Vec<u8> {
    fn put(out: &mut Vec<u8>, c: &Chunk) {
        match c {
            Chunk::Leaf(id, p) => frame(out, id, p),
            Chunk::List(kind, kids) => {
                let mut body = kind.to_vec();
                kids.iter().for_each(|k| put(&mut body, k));
                frame(out, b"LIST", &body);
            }
        }
    }
    let mut body = form.to_vec();
    children.iter().for_each(|k| put(&mut body, k));
    let mut out = Vec::new();
    frame(&mut out, b"RIFF", &body);
    out
}

/// The parts of a split-layout document: the chunk stream and the data files it refers to.
#[derive(Debug, Clone, PartialEq)]
pub struct Parts {
    /// `content/root.dat`.
    pub root: Vec<u8>,
    /// `content/data/<name>` in `content/dataFileList.dat` order.
    pub data: Vec<(String, Vec<u8>)>,
    /// `previews/thumbnail.png`, when present.
    pub thumbnail: Option<Vec<u8>>,
    /// Other ZIP entries by name (`font/fontTable.dat`, `color/docPalette.xml`…).
    pub extra: Vec<(String, Vec<u8>)>,
}

impl Parts {
    /// Serialise a chunk tree in the split layout. Payloads of at most 8 bytes are inline; the
    /// others go to `page<n>.dat` inside the n-th top-level page list and to `data1.dat` elsewhere.
    pub fn split(form: &[u8; 4], children: &[Chunk]) -> Self {
        struct W {
            data: Vec<(String, Vec<u8>)>,
        }
        impl W {
            fn file(&mut self, name: &str) -> usize {
                if let Some(i) = self.data.iter().position(|(n, _)| n == name) {
                    return i;
                }
                self.data.push((name.to_string(), Vec::new()));
                self.data.len() - 1
            }
            fn put(&mut self, out: &mut Vec<u8>, c: &Chunk, file: &str) {
                match c {
                    Chunk::Leaf(id, p) => {
                        let mut r = Vec::with_capacity(16);
                        if p.len() <= 8 {
                            r.extend((-1i32).to_le_bytes());
                            r.extend((p.len() as u32).to_le_bytes());
                            r.extend(p);
                            r.resize(16, 0);
                        } else {
                            let i = self.file(file);
                            let data = &mut self.data[i].1;
                            r.extend((i as i32).to_le_bytes());
                            r.extend((p.len() as u32).to_le_bytes());
                            r.extend((data.len() as u32).to_le_bytes());
                            r.extend(0u32.to_le_bytes());
                            data.extend(p);
                        }
                        frame(out, id, &r);
                    }
                    Chunk::List(kind, kids) => {
                        let mut body = kind.to_vec();
                        kids.iter().for_each(|k| self.put(&mut body, k, file));
                        frame(out, b"LIST", &body);
                    }
                }
            }
        }
        let mut w = W { data: Vec::new() };
        let mut body = form.to_vec();
        let mut pages = 0;
        for c in children {
            let file = match c {
                Chunk::List([b'p', b'a', b'g', b'e'], _) => {
                    pages += 1;
                    format!("page{pages}.dat")
                }
                _ => "data1.dat".to_string(),
            };
            w.put(&mut body, c, &file);
        }
        let mut root = Vec::new();
        frame(&mut root, b"RIFF", &body);
        Self { root, data: w.data, thumbnail: None, extra: Vec::new() }
    }

    /// `content/dataFileList.dat`: one name per line, no trailing newline.
    pub fn file_list(&self) -> Vec<u8> {
        self.data.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join("\n").into_bytes()
    }

    /// The ZIP document: `mimetype` (stored, first), `META-INF/container.xml`, the stream, the
    /// file list, the data files, the extra entries and the thumbnail (stored), every other entry
    /// with `method`.
    pub fn zip(&self, method: Method) -> Vec<u8> {
        let list = self.file_list();
        let names: Vec<String> = self.data.iter().map(|(n, _)| format!("content/data/{n}")).collect();
        let mut entries: Vec<(&str, &[u8], Method)> = vec![
            ("mimetype", MIMETYPE.as_bytes(), Method::Stored),
            ("META-INF/container.xml", CONTAINER_XML.as_bytes(), method),
            ("content/root.dat", &self.root, method),
            ("content/dataFileList.dat", &list, method),
        ];
        for (name, (_, data)) in names.iter().zip(&self.data) {
            entries.push((name, data, method));
        }
        for (name, data) in &self.extra {
            entries.push((name, data, method));
        }
        if let Some(png) = &self.thumbnail {
            entries.push(("previews/thumbnail.png", png, Method::Stored));
        }
        zip(&entries)
    }
}

/// The `mimetype` the synthetic documents carry.
pub const MIMETYPE: &str = "application/x-vnd.corel.zcf.draw.document+zip";

const CONTAINER_XML: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8"?><container version="1.0" xmlns:crl="urn:example:synth">"#,
    r#"<rootfiles><rootfile crl:file-kind="main" full-path="content/root.dat"/>"#,
    r#"<rootfile crl:file-kind="thumbnail" full-path="previews/thumbnail.png" media-type="image/png"/></rootfiles></container>"#
);

/// A valid, decodable 8-bit RGB PNG of one colour.
pub fn png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend(data);
        out.extend(&c);
        out.extend(crc32fast::hash(&c).to_be_bytes());
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    let mut raw = Vec::new();
    let row: Vec<u8> = std::iter::once(0).chain((0..width).flat_map(|_| rgb)).collect();
    for _ in 0..height {
        raw.extend(&row);
    }
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(&raw).unwrap();
    chunk(&mut out, b"IDAT", &e.finish().unwrap());
    chunk(&mut out, b"IEND", &[]);
    out
}

// ---------------------------------------------------------------------------------------------
// Records

/// `loda`: header, argument offsets (with the end sentinel) and type ids in reverse order.
pub fn loda(obj_type: u32, args: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let n = args.len();
    let types_start = 20 + 4 * (n + 1);
    let data_start = types_start + 4 * n;
    let length = data_start + args.iter().map(|(_, d)| d.len()).sum::<usize>();
    let mut out = Vec::new();
    for v in [length, n, 20, types_start] {
        out.extend((v as u32).to_le_bytes());
    }
    out.extend(obj_type.to_le_bytes());
    let mut at = data_start;
    for (_, d) in args {
        out.extend((at as u32).to_le_bytes());
        at += d.len();
    }
    out.extend((at as u32).to_le_bytes());
    for (t, _) in args.iter().rev() {
        out.extend(t.to_le_bytes());
    }
    for (_, d) in args {
        out.extend(d);
    }
    out
}

/// `trfd` with one affine record, CorelDRAW order: `x' = a·x + b·y + c`, `y' = d·x + e·y + f`.
pub fn trfd(m: [f64; 6]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [96u32, 1, 0x10, 0xFFFF, 0x18, 0x60, 0x6CC, 56] {
        out.extend(v.to_le_bytes());
    }
    out.extend(8u16.to_le_bytes());
    out.extend([0u8; 6]);
    m.iter().for_each(|v| out.extend(v.to_le_bytes()));
    out.extend([0u8; 8]);
    out
}

pub const IDENTITY: [f64; 6] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];

/// A translation in world units.
pub fn translate(x: f64, y: f64) -> [f64; 6] {
    [1.0, 0.0, x, 0.0, 1.0, y]
}

/// `bbox`: xMin, yMax, xMax, yMin.
pub fn bbox(b: [i32; 4]) -> Vec<u8> {
    b.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn u32le(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// UTF-16LE with its NUL terminator (loda arg 1000).
pub fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

/// A tagged colour block: value (tag 1), an all-zero palette GUID (tag 7), terminator.
pub fn color_block(model: u16, values: [u8; 4]) -> Vec<u8> {
    let mut out = vec![1];
    out.extend(12u32.to_le_bytes());
    out.extend(model.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(values);
    out.push(7);
    out.extend(16u32.to_le_bytes());
    out.extend([0u8; 16]);
    out.push(0);
    out.extend(0u32.to_le_bytes());
    out
}

/// `mcfg` with the page size at 12/16 and nothing else meaningful.
pub fn mcfg(width: i32, height: i32) -> Vec<u8> {
    let mut out = vec![0u8; 40];
    out[0..4].copy_from_slice(&2100u32.to_le_bytes());
    out[4..8].copy_from_slice(&32u32.to_le_bytes());
    out[12..16].copy_from_slice(&width.to_le_bytes());
    out[16..20].copy_from_slice(&height.to_le_bytes());
    out
}

/// Path node types.
pub const MOVE: u8 = 0x0C;
pub const LINE: u8 = 0x44;
pub const CONTROL: u8 = 0xC0;
pub const CURVE_END: u8 = 0x84;
/// Bit 3: closes the subpath (on its last node).
pub const CLOSE: u8 = 0x08;

/// A path payload: point count, points, node types.
pub fn path(nodes: &[(i32, i32, u8)]) -> Vec<u8> {
    let mut out = (nodes.len() as u32).to_le_bytes().to_vec();
    for (x, y, _) in nodes {
        out.extend(x.to_le_bytes());
        out.extend(y.to_le_bytes());
    }
    out.extend(nodes.iter().map(|n| n.2));
    out
}

/// A rectangle payload: size, scale and four equal corners (type 0, radius fraction).
pub fn rectangle(w: f64, h: f64, radius_fraction: f64) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [w, h, 1.0, 1.0] {
        out.extend(v.to_le_bytes());
    }
    for _ in 0..4 {
        out.extend(0i32.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend(radius_fraction.to_le_bytes());
        out.extend([0u8; 8]);
    }
    out
}

/// An ellipse payload: size, start and end angle in micro-degrees, arc kind.
pub fn ellipse(w: i32, h: i32, start: i32, end: i32, arc_kind: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [w, h, start, end] {
        out.extend(v.to_le_bytes());
    }
    out.extend(arc_kind.to_le_bytes());
    out
}

/// A bitmap object payload: the frame, the picture size, the `bmp ` id and the frame as clip path.
pub fn bitmap_record(frame: [i32; 4], width: i32, height: i32, id: u32) -> Vec<u8> {
    let [x0, top, x1, bottom] = frame;
    let mut out = Vec::new();
    for v in frame.iter().chain(frame.iter()) {
        out.extend(v.to_le_bytes());
    }
    out.extend(2u16.to_le_bytes());
    out.extend(24u16.to_le_bytes());
    out.extend(width.to_le_bytes());
    out.extend(height.to_le_bytes());
    out.extend(0i32.to_le_bytes());
    out.extend(id.to_le_bytes());
    for v in [0i32, 0, -1, 0, 0] {
        out.extend(v.to_le_bytes());
    }
    out.extend(path(&[(x0, top, MOVE), (x1, top, LINE), (x1, bottom, LINE), (x0, bottom, LINE), (x0, top, LINE | CLOSE)]));
    out.extend(0u32.to_le_bytes());
    out
}

// ---------------------------------------------------------------------------------------------
// Paint records

/// A 12-byte colour value: model, palette type 5, palette index 0, four component bytes (RGB
/// models store B, G, R, 0).
pub fn color_value(model: u16, values: [u8; 4]) -> Vec<u8> {
    let mut out = model.to_le_bytes().to_vec();
    out.extend(5u16.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(values);
    out
}

/// A tagged colour block carrying the GUID (as stored) of the palette the colour came from.
pub fn color_block_in(model: u16, values: [u8; 4], palette: [u8; 16]) -> Vec<u8> {
    let mut out = vec![1];
    out.extend(12u32.to_le_bytes());
    out.extend(color_value(model, values));
    out.push(7);
    out.extend(16u32.to_le_bytes());
    out.extend(palette);
    out.push(0);
    out.extend(0u32.to_le_bytes());
    out
}

/// An RGB colour block (stored B, G, R).
pub fn rgb(r: u8, g: u8, b: u8) -> Vec<u8> {
    color_block(5, [b, g, r, 0])
}

/// A grey colour block (0 black .. 255 white).
pub fn gray(v: u8) -> Vec<u8> {
    color_block(9, [v, 0, 0, 0])
}

/// `{u32 tag, u32 len, data}`.
fn item(tag: u32, data: &[u8]) -> Vec<u8> {
    let mut out = tag.to_le_bytes().to_vec();
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    out
}

/// The 16-byte halftone screen block (45°, 60 lpi).
fn screen() -> Vec<u8> {
    let mut out = vec![0u8; 8];
    out.extend(45_000_000u32.to_le_bytes());
    out.extend(60u32.to_le_bytes());
    out
}

/// A record GUID; the reader never looks at it.
const RECORD_GUID: [u8; 16] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x48, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x01];

fn fill_record(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = id.to_le_bytes().to_vec();
    out.extend(2400u32.to_le_bytes());
    out.extend(16u32.to_le_bytes());
    out.extend(RECORD_GUID);
    out.extend(item(1300, body));
    out.extend([0u8; 8]);
    out
}

/// A `fild` with no fill.
pub fn fild_none(id: u32) -> Vec<u8> {
    fill_record(id, &0u16.to_le_bytes())
}

/// A `fild` of fill type `fill_type` whose body is one inner item 1300 (uniform fills use 1;
/// other types are what the reader reports as unsupported).
pub fn fild_type(id: u32, fill_type: u16, inner: &[u8]) -> Vec<u8> {
    let mut body = fill_type.to_le_bytes().to_vec();
    body.extend(item(1300, inner));
    body.extend([0u8; 8]);
    fill_record(id, &body)
}

/// A uniform `fild` of one colour block.
pub fn fild_uniform(id: u32, color_block: &[u8]) -> Vec<u8> {
    let mut inner = color_block.to_vec();
    inner.extend(screen());
    fild_type(id, 1, &inner)
}

/// One colour of a fountain.
#[derive(Debug, Clone, PartialEq)]
pub struct FountainStop {
    pub color: Vec<u8>,
    /// 0..100.
    pub position: u16,
    pub alpha: u8,
    /// Midpoint (0..100) of the segment after this stop.
    pub midpoint: u16,
}

pub fn stop(color: Vec<u8>, position: u16) -> FountainStop {
    FountainStop { color, position, alpha: 255, midpoint: 50 }
}

/// A fountain fill: kind (1 linear, 2 radial), colours, centre offset and unit-space matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct Fountain {
    pub kind: u16,
    pub stops: Vec<FountainStop>,
    pub centre: (f64, f64),
    /// `x' = m0·x + m1·y + m2`, `y' = m3·x + m4·y + m5`.
    pub matrix: [f64; 6],
}

/// A fountain `fild`: body item 1300 (header and colours) and placement item 1700.
pub fn fild_fountain(id: u32, f: &Fountain) -> Vec<u8> {
    let mut head = f.kind.to_le_bytes().to_vec();
    head.extend(screen());
    head.extend(0u16.to_le_bytes()); // edge pad
    head.extend(0u32.to_le_bytes()); // angle (the matrix is what counts)
    head.extend(0u16.to_le_bytes());
    head.extend([0u8; 8]);
    head.extend((if f.stops.len() > 2 { 3u16 } else { 0 }).to_le_bytes());
    head.extend((if f.kind == 2 { 1u16 } else { 0 }).to_le_bytes());
    head.extend(f.stops.first().map_or(50, |s| s.midpoint).to_le_bytes());
    head.extend((f.stops.len() as u16).to_le_bytes());
    for s in &f.stops {
        head.extend(&s.color);
        head.extend(s.position.to_le_bytes());
    }
    let mut place = Vec::new();
    for v in [f.centre.0, f.centre.1].iter().chain(f.matrix.iter()) {
        place.extend(v.to_le_bytes());
    }
    place.extend(0u32.to_le_bytes()); // anisotropic
    place.extend([0u8; 12]);
    for s in &f.stops {
        place.push(s.alpha);
        place.extend(0u16.to_le_bytes());
        place.extend(s.midpoint.to_le_bytes());
    }
    let mut body = 2u16.to_le_bytes().to_vec();
    body.extend(item(1300, &head));
    body.extend(item(1700, &place));
    body.extend([0u8; 8]);
    fill_record(id, &body)
}

/// An outline pen.
#[derive(Debug, Clone, PartialEq)]
pub struct Pen {
    /// 0x01 off, 0x02 drawn, 0x10 behind the fill, 0x20 scales with the object.
    pub flags: u32,
    /// 0 miter, 1 round, 2 bevel.
    pub join: u16,
    /// World units.
    pub width: u32,
    pub stretch: u16,
    /// A colour block.
    pub color: Vec<u8>,
    pub miter_micro_degrees: u32,
    /// The undecoded caps, dash and arrowhead area.
    pub extras: [u8; 38],
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            flags: 0x02,
            join: 0,
            width: 10_000,
            stretch: 100,
            color: color_block(2, [0, 0, 0, 100]),
            miter_micro_degrees: 5_000_000,
            extras: [0; 38],
        }
    }
}

/// An `outl` record: miter (item 3), colour (item 2) and pen (item 1).
pub fn outl(id: u32, pen: &Pen) -> Vec<u8> {
    let mut p = pen.flags.to_le_bytes().to_vec();
    p.extend(pen.join.to_le_bytes());
    p.extend(pen.width.to_le_bytes());
    p.extend(pen.stretch.to_le_bytes());
    p.extend(0i32.to_le_bytes());
    for v in IDENTITY {
        p.extend(v.to_le_bytes());
    }
    // The pen repeats the colour value without its tags.
    p.extend(pen.color.get(5..17).map_or_else(|| vec![0; 12], <[u8]>::to_vec));
    p.extend(screen());
    p.extend(pen.extras);
    let mut out = id.to_le_bytes().to_vec();
    out.extend(11u32.to_le_bytes());
    out.extend(16u32.to_le_bytes());
    out.extend(RECORD_GUID);
    out.extend(item(3, &pen.miter_micro_degrees.to_le_bytes()));
    out.extend(item(2, &pen.color));
    out.extend(item(1, &p));
    out.extend([0u8; 8]);
    out
}

/// The pen that draws nothing (flag 0x01).
pub fn outl_none(id: u32) -> Vec<u8> {
    outl(id, &Pen { flags: 0x01, ..Pen::default() })
}

/// `doc/filt`: one `filc` list per fill record.
pub fn filt(records: Vec<Vec<u8>>) -> Chunk {
    list(b"filt", records.into_iter().map(|r| list(b"filc", vec![leaf(b"fild", r)])).collect())
}

/// `doc/otlt`.
pub fn otlt(records: Vec<Vec<u8>>) -> Chunk {
    list(b"otlt", records.into_iter().map(|r| leaf(b"outl", r)).collect())
}

/// Loda arg 8000: transparency `start`/`end` (0..100), merge mode and the fill record holding the
/// colour or grey ramp.
pub fn transparency(start: f64, end: f64, mode: u32, fill_id: u32) -> Vec<u8> {
    let mut body = vec![5];
    body.extend(7u32.to_le_bytes());
    body.extend([0u8; 24]);
    body.extend(1u32.to_le_bytes());
    body.extend((-1i32).to_le_bytes());
    body.extend(0u32.to_le_bytes());
    body.extend([0u8; 16]);
    body.extend((-1i32).to_le_bytes());
    body.extend(start.to_le_bytes());
    body.extend(end.to_le_bytes());
    body.extend(mode.to_le_bytes());
    body.extend(2u32.to_le_bytes());
    body.extend([0u8; 20]);
    body.extend(fill_id.to_le_bytes());
    body.extend([0u8; 8]);
    item(1300, &body)
}

/// A 48-byte tile matrix (`ftil`, `ttil`).
pub fn tile(m: [f64; 6]) -> Vec<u8> {
    m.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A font record of a style table: record id, font ids, sizes (world units) and weight bit fields
/// for the latin, far-east and right-to-left slots.
pub type FontStyle = (u32, [u32; 3], [u32; 3], [u32; 3]);

/// A style table: (record id, fill id), (record id, outline id), (record id, font ids, sizes,
/// weights), (record id, alignment), (record id, character spacing ×1e6), and styles (style id,
/// record ids for fill, outline, font, align, interval, decoration, misc).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StyleTable {
    pub fills: Vec<(u32, u32)>,
    pub outlines: Vec<(u32, u32)>,
    pub fonts: Vec<FontStyle>,
    pub aligns: Vec<(u32, u32)>,
    pub intervals: Vec<(u32, i32)>,
    pub styles: Vec<(u32, [u32; 7])>,
}

/// `doc/stlt`.
pub fn stlt(t: &StyleTable) -> Vec<u8> {
    let mut out = (t.styles.len() as u32).to_le_bytes().to_vec();
    let u = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    u(&mut out, t.fills.len() as u32);
    for (id, fill) in &t.fills {
        for v in [*id, 0, *fill] {
            u(&mut out, v);
        }
        for v in IDENTITY {
            out.extend(v.to_le_bytes());
        }
    }
    u(&mut out, t.outlines.len() as u32);
    for (id, outline) in &t.outlines {
        for v in [*id, 0, *outline] {
            u(&mut out, v);
        }
    }
    u(&mut out, t.fonts.len() as u32);
    for (id, fonts, sizes, weights) in &t.fonts {
        for v in [*id, 0, 255, 255, 255, 0].iter().chain(fonts).chain(sizes).chain(weights) {
            u(&mut out, *v);
        }
    }
    u(&mut out, t.aligns.len() as u32);
    for (id, value) in &t.aligns {
        for v in [*id, 0, *value] {
            u(&mut out, v);
        }
    }
    u(&mut out, t.intervals.len() as u32);
    for (id, spacing) in &t.intervals {
        for v in [*id, 0, 1023] {
            u(&mut out, v);
        }
        for v in [*spacing, 0, 1_000_000 + spacing, 1_000_000, 1_000_000, 0, 2_000_000, 600_000, 3_000_000, 0] {
            out.extend(v.to_le_bytes());
        }
    }
    u(&mut out, 0); // decorations
    u(&mut out, 0); // misc
    for (id, refs) in &t.styles {
        for v in [2, *id, 0, 0, 0, 0].iter().chain(refs) {
            u(&mut out, *v);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Text

/// A text story: placement (CorelDRAW order), style id, object and character style JSON, and
/// the text bytes (Windows-1252). `chars` overrides the character count and `records` the
/// per-character records (8 bytes each, zero by default).
#[derive(Debug, Clone, PartialEq)]
pub struct Story {
    pub matrix: [f64; 6],
    pub style_id: u32,
    pub style: String,
    pub char_style: String,
    pub text: Vec<u8>,
    pub chars: Option<u32>,
    pub records: Option<Vec<u8>>,
}

impl Story {
    pub fn new(text: &[u8], style: String) -> Self {
        Self { matrix: IDENTITY, style_id: 1, style, char_style: r#"{"character":{}}"#.into(), text: text.to_vec(), chars: None, records: None }
    }
}

/// A `txsm` story record.
pub fn txsm(s: &Story) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [1u32, 1, 0, 1, 8, 14, 0, 3, 0, 1800] {
        out.extend(v.to_le_bytes());
    }
    out.push(0);
    out.extend(1u32.to_le_bytes());
    out.extend(1u32.to_le_bytes()); // text id
    for v in s.matrix {
        out.extend(v.to_le_bytes());
    }
    out.extend([0u8; 12]);
    out.extend(1u32.to_le_bytes());
    out.extend(s.style_id.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((s.style.len() as u32).to_le_bytes());
    out.extend(s.style.as_bytes());
    let chars = s.chars.unwrap_or(s.text.len() as u32);
    out.extend(1u32.to_le_bytes());
    out.extend((chars + 1).to_le_bytes());
    out.extend(0x96E4u16.to_le_bytes());
    out.extend(3u32.to_le_bytes());
    out.extend("XXX".encode_utf16().flat_map(u16::to_le_bytes));
    out.extend((s.char_style.len() as u32).to_le_bytes());
    out.extend(s.char_style.as_bytes());
    out.extend(chars.to_le_bytes());
    match &s.records {
        Some(r) => out.extend(r),
        None => out.extend(vec![0u8; 8 * chars as usize]),
    }
    out.extend((s.text.len() as u32).to_le_bytes());
    out.extend(&s.text);
    out.extend([0u8; 5]);
    out
}

/// A minimal object style: latin family, size (world units), weight bit field, fill colour
/// string and character spacing (×1e6 of the space width).
pub fn style_json(family: &str, size: u32, weight: u32, fill: &str, spacing: i32) -> String {
    format!(
        r#"{{"character":{{"latin":{{"font":"{family}","size":"{size}","weight":"{weight}","italic":"0","underline":"0"}},"fill":{{"type":"1","primaryColor":"{fill}"}},"outline":{{"width":"0"}}}},"paragraph":{{"interCharSpacing":"{spacing}"}}}}"#
    )
}

fn fixed_utf16(s: &str, units: usize) -> Vec<u8> {
    let mut v: Vec<u8> = s.encode_utf16().take(units - 1).flat_map(u16::to_le_bytes).collect();
    v.resize(2 * units, 0);
    v
}

/// A `doc/fntt/font` record (296 bytes).
pub fn font(id: u32, weight: u32, family: &str, postscript: &str) -> Vec<u8> {
    let mut out = id.to_le_bytes().to_vec();
    out.extend(weight.to_le_bytes());
    out.extend([0u8; 10]);
    out.extend(fixed_utf16(family, 32));
    out.extend(0u32.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // charset
    out.extend(fixed_utf16("Western", 32));
    out.extend(0u32.to_le_bytes());
    out.extend(1u32.to_le_bytes());
    out.extend(2u32.to_le_bytes());
    out.extend(fixed_utf16(postscript, 64));
    out.extend(0u32.to_le_bytes());
    out
}

/// `doc/fntt`.
pub fn fntt(records: Vec<Vec<u8>>) -> Chunk {
    list(b"fntt", records.into_iter().map(|r| leaf(b"font", r)).collect())
}

/// `font/fontTable.dat` with (font id, weight, family, PostScript name) records.
pub fn font_table(fonts: &[(u32, u32, &str, &str)]) -> Vec<u8> {
    let counted = |s: &str| {
        let units: Vec<u16> = s.encode_utf16().chain([0]).collect();
        let mut v = (units.len() as u32).to_le_bytes().to_vec();
        v.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        v
    };
    let mut body = (fonts.len() as u32).to_le_bytes().to_vec();
    for (id, weight, family, postscript) in fonts {
        let mut r = id.to_le_bytes().to_vec();
        r.extend(weight.to_le_bytes());
        r.extend([0u8; 10]);
        r.extend(counted(family));
        r.push(0);
        r.extend(counted("Western"));
        r.extend(1u32.to_le_bytes());
        r.extend(2u32.to_le_bytes());
        r.extend(counted(postscript));
        r.extend(counted("1.0"));
        r.extend(counted("Regular"));
        body.extend(0u32.to_le_bytes());
        body.extend((r.len() as u32).to_le_bytes());
        body.extend(r);
    }
    let mut out = 0u32.to_le_bytes().to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

/// `color/docPalette.xml` with a palette GUID (as written in the XML) and (`CMYK`|`RGB`, name,
/// components 0..1) entries.
pub fn doc_palette(guid: &str, name: &str, entries: &[(&str, &str, &[f64])]) -> Vec<u8> {
    let mut xml = format!(r#"<?xml version="1.0"?><palette guid="{guid}" name="{name}"><colors><page>"#);
    for (cs, n, tints) in entries {
        let t: Vec<String> = tints.iter().map(|v| format!("{v}")).collect();
        xml.push_str(&format!(r#"<color cs="{cs}" name="{n}" tints="{}"/>"#, t.join(",")));
    }
    xml.push_str("</page></colors></palette>");
    xml.into_bytes()
}

// ---------------------------------------------------------------------------------------------
// Bitmaps

/// Picture content, top row first.
#[derive(Debug, Clone, PartialEq)]
pub enum Pixels {
    /// RGB pixels (stored as 24-bit B, G, R).
    Rgb(Vec<[u8; 3]>),
    /// An RGB palette (stored B, G, R) and one index per pixel.
    Indexed(Vec<[u8; 3]>, Vec<u8>),
}

/// One RI block. `palette` entries are RGB. Rows go bottom-up, padded to 4 bytes with garbage.
fn ri_block(model: u32, width: u32, height: u32, bits: u32, rows: &[Vec<u8>], palette: Option<&[[u8; 3]]>, resolution: u32) -> Vec<u8> {
    let stride = (width * bits).div_ceil(32) * 4;
    let palette_bytes: Vec<u8> = palette.map_or_else(Vec::new, |p| {
        let mut v = 4u16.to_le_bytes().to_vec();
        v.extend((p.len() as u16).to_le_bytes());
        v.extend(p.iter().flat_map(|[r, g, b]| [*b, *g, *r]));
        v
    });
    let data_off = 14 + 64 + palette_bytes.len() as u32;
    let image_size = stride * height;
    let mut out = 0x4952u16.to_le_bytes().to_vec();
    out.extend((data_off + image_size).to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(data_off.to_le_bytes());
    let palette_off = if palette.is_some() { 64u32 } else { 0 };
    for v in [model, 1, width, height, 1, bits, stride, image_size, 0, resolution, resolution, palette_off, data_off - 14, 0, 0, 0] {
        out.extend(v.to_le_bytes());
    }
    out.extend(palette_bytes);
    for row in rows.iter().rev() {
        let mut r = row.clone();
        r.resize(stride as usize, 0xAB);
        out.extend(r);
    }
    out
}

/// A `bmp ` record: a picture and an optional alpha mask (one byte per pixel, top row first).
/// Resolution 72 dpi.
pub fn bmp(id: u32, width: u32, height: u32, pixels: &Pixels, mask: Option<&[u8]>) -> Vec<u8> {
    let w = width as usize;
    let image = match pixels {
        Pixels::Rgb(px) => {
            let rows: Vec<Vec<u8>> = px.chunks(w).map(|r| r.iter().flat_map(|[r, g, b]| [*b, *g, *r]).collect()).collect();
            ri_block(1, width, height, 24, &rows, None, 2_834_645)
        }
        Pixels::Indexed(palette, idx) => {
            let rows: Vec<Vec<u8>> = idx.chunks(w).map(<[u8]>::to_vec).collect();
            ri_block(10, width, height, 8, &rows, Some(palette), 2_834_645)
        }
    };
    let mask_block = mask.map(|m| {
        let rows: Vec<Vec<u8>> = m.chunks(w).map(<[u8]>::to_vec).collect();
        ri_block(99, width, height, 8, &rows, None, 0)
    });
    let mask_off = mask_block.as_ref().map_or(0, |_| 0x20 + image.len() as u32);
    let mut body = Vec::new();
    for v in [0u32, 0x20, mask_off, 0, 2, 0xFF] {
        body.extend(v.to_le_bytes());
    }
    body.extend(image);
    body.extend(mask_block.unwrap_or_default());
    let mut out = id.to_le_bytes().to_vec();
    out.extend(1u32.to_le_bytes());
    out.extend(0x4955u32.to_le_bytes());
    out.extend((body.len() as u32 + 8).to_le_bytes());
    out.extend(body);
    out
}

/// The root `bmpt` list.
pub fn bmpt(records: Vec<Vec<u8>>) -> Chunk {
    list(b"bmpt", records.into_iter().map(|r| leaf(b"bmp ", r)).collect())
}

/// A guideline payload (loda arg 16000) through two points.
pub fn guideline(a: (i32, i32), b: (i32, i32), angle: i32) -> Vec<u8> {
    let mut out = vec![0u8; 12];
    for v in [angle, a.0, a.1, b.0, b.1, b.0, b.1] {
        out.extend(v.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Nodes

/// `loda` argument ids and object types the builders use.
pub mod ids {
    pub const ARG_OUTLINE: u32 = 10;
    pub const ARG_FILL: u32 = 20;
    pub const ARG_GEOMETRY: u32 = 30;
    pub const ARG_SERIAL: u32 = 100;
    pub const ARG_NAME: u32 = 1000;
    pub const ARG_LAYER_COLOR: u32 = 2000;
    pub const ARG_TRANSPARENCY: u32 = 8000;
    pub const ARG_POWERCLIP: u32 = 8005;
    pub const ARG_GUIDELINE: u32 = 16000;
    pub const ARG_LAYER_GUID: u32 = 40050;

    pub const RECTANGLE: u32 = 1;
    pub const ELLIPSE: u32 = 2;
    pub const CURVE: u32 = 3;
    pub const TEXT: u32 = 4;
    pub const BITMAP: u32 = 5;
    pub const GRID_LAYER: u32 = 11;
    pub const GUIDES_LAYER: u32 = 12;
    pub const DESKTOP_LAYER: u32 = 17;
    pub const GUIDELINE: u32 = 22;
    pub const PLACEHOLDER: u32 = 31;
}

/// An `obj ` list to build.
#[derive(Debug, Clone, PartialEq)]
pub struct Obj {
    pub obj_type: u32,
    pub args: Vec<(u32, Vec<u8>)>,
    /// Local → world (CorelDRAW order).
    pub matrix: [f64; 6],
    pub flags: u32,
    pub spnd: Option<u32>,
    pub bbox: [i32; 4],
    /// `lgob` children between `loda` and `trfl` (`ftil`, `ttil`).
    pub lgob_extra: Vec<Chunk>,
    /// Children after `lgob` (`txsm`, `txtj`).
    pub extra: Vec<Chunk>,
    /// Local points used to compute `bbox` (their hull through `matrix`).
    hull: Vec<(f64, f64)>,
}

impl Obj {
    fn new(obj_type: u32, geometry: Option<Vec<u8>>, hull: Vec<(f64, f64)>) -> Self {
        let mut args = vec![(ids::ARG_OUTLINE, u32le(1)), (ids::ARG_FILL, u32le(1)), (ids::ARG_SERIAL, u32le(1))];
        if let Some(g) = geometry {
            args.push((ids::ARG_GEOMETRY, g));
        }
        let mut o =
            Self { obj_type, args, matrix: IDENTITY, flags: 0x0800_0200, spnd: None, bbox: [0; 4], lgob_extra: Vec::new(), extra: Vec::new(), hull };
        o.update_bbox();
        o
    }

    fn update_bbox(&mut self) {
        let [a, b, c, d, e, f] = self.matrix;
        let pts: Vec<(f64, f64)> = self.hull.iter().map(|(x, y)| (a * x + b * y + c, d * x + e * y + f)).collect();
        if pts.is_empty() {
            self.bbox = [-2, 2, 2, -2];
            return;
        }
        let min_x = pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let max_x = pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
        let min_y = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_y = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        self.bbox = [min_x.round() as i32, max_y.round() as i32, max_x.round() as i32, min_y.round() as i32];
    }

    pub fn curve(nodes: &[(i32, i32, u8)]) -> Self {
        Self::new(ids::CURVE, Some(path(nodes)), nodes.iter().map(|(x, y, _)| (f64::from(*x), f64::from(*y))).collect())
    }

    /// A closed polygon through `points`.
    pub fn polygon(points: &[(i32, i32)]) -> Self {
        let mut nodes: Vec<(i32, i32, u8)> = points.iter().enumerate().map(|(i, (x, y))| (*x, *y, if i == 0 { MOVE } else { LINE })).collect();
        if let Some(&(x, y)) = points.first() {
            nodes.push((x, y, LINE | CLOSE));
        }
        Self::curve(&nodes)
    }

    pub fn rectangle(w: f64, h: f64, radius_fraction: f64) -> Self {
        Self::new(ids::RECTANGLE, Some(rectangle(w, h, radius_fraction)), vec![(0.0, 0.0), (w, h)])
    }

    pub fn ellipse(w: i32, h: i32) -> Self {
        Self::arc(w, h, 270_000_000, 270_000_000, 0)
    }

    pub fn arc(w: i32, h: i32, start: i32, end: i32, arc_kind: u32) -> Self {
        Self::new(ids::ELLIPSE, Some(ellipse(w, h, start, end, arc_kind)), vec![(0.0, 0.0), (f64::from(w), f64::from(h))])
    }

    /// Artistic text (its story is not written: the reader's text stage decodes `txsm`).
    pub fn text() -> Self {
        let mut payload = 0u32.to_le_bytes().to_vec();
        payload.extend(0u32.to_le_bytes());
        Self::new(ids::TEXT, Some(payload), vec![(0.0, 0.0), (100_000.0, 50_000.0)])
    }

    pub fn bitmap(frame: [i32; 4], width: i32, height: i32, id: u32) -> Self {
        let hull = vec![(f64::from(frame[0]), f64::from(frame[1])), (f64::from(frame[2]), f64::from(frame[3]))];
        Self::new(ids::BITMAP, Some(bitmap_record(frame, width, height, id)), hull)
    }

    /// A property-only object without geometry.
    pub fn placeholder() -> Self {
        let mut o = Self::new(ids::PLACEHOLDER, None, Vec::new());
        o.flags = 0x0800_0158;
        o
    }

    /// A vertical guideline at `x`.
    pub fn guideline(x: i32) -> Self {
        let mut o = Self::new(ids::GUIDELINE, Some(vec![0; 12]), Vec::new());
        o.args.push((ids::ARG_GUIDELINE, guideline((x, -228_600_000), (x, 228_600_000), 90_000_000)));
        o.bbox = [x - 2, 228_600_000, x + 2, -228_600_000];
        o
    }

    /// Place the object (local → world).
    pub fn at(mut self, m: [f64; 6]) -> Self {
        self.matrix = m;
        self.update_bbox();
        self
    }

    pub fn arg(mut self, id: u32, data: Vec<u8>) -> Self {
        self.args.retain(|(t, _)| *t != id);
        self.args.push((id, data));
        self
    }

    pub fn without(mut self, id: u32) -> Self {
        self.args.retain(|(t, _)| *t != id);
        self
    }

    pub fn named(self, name: &str) -> Self {
        self.arg(ids::ARG_NAME, utf16z(name))
    }

    /// Use fill record `id`.
    pub fn fill(self, id: u32) -> Self {
        self.arg(ids::ARG_FILL, u32le(id))
    }

    /// Use outline record `id`.
    pub fn outline(self, id: u32) -> Self {
        self.arg(ids::ARG_OUTLINE, u32le(id))
    }

    /// A transparency (see [`transparency`]) with its `ttil` tile matrix (identity).
    pub fn transparent(mut self, record: Vec<u8>) -> Self {
        self.lgob_extra.retain(|c| !matches!(c, Chunk::Leaf(id, _) if id == b"ttil"));
        self.lgob_extra.push(leaf(b"ttil", tile(IDENTITY)));
        self.arg(ids::ARG_TRANSPARENCY, record)
    }

    /// A `lgob` child such as `ftil` or `ttil`.
    pub fn lgob(mut self, id: &[u8; 4], payload: Vec<u8>) -> Self {
        self.lgob_extra.retain(|c| !matches!(c, Chunk::Leaf(i, _) if i == id));
        self.lgob_extra.push(leaf(id, payload));
        self
    }

    /// The `txsm` story of a text object (see [`txsm`]).
    pub fn story(mut self, payload: Vec<u8>) -> Self {
        self.extra.retain(|c| !matches!(c, Chunk::Leaf(id, _) if id == b"txsm"));
        self.extra.push(leaf(b"txsm", payload));
        self
    }

    /// Make this object a PowerClip container of the `clpt` group with this `spnd` key.
    pub fn powerclip(mut self, key: u32) -> Self {
        let mut record = key.to_le_bytes().to_vec();
        for v in [1u32, 0, 1] {
            record.extend(v.to_le_bytes());
        }
        self.flags |= 0x0010_0000;
        self.arg(ids::ARG_POWERCLIP, record)
    }

    pub fn chunk(&self) -> Chunk {
        let mut kids = Vec::new();
        if let Some(s) = self.spnd {
            kids.push(leaf(b"spnd", u32le(s)));
        }
        kids.extend(common(self.flags, self.bbox));
        let mut lgob = vec![leaf(b"loda", loda(self.obj_type, &self.args))];
        lgob.extend(self.lgob_extra.iter().cloned());
        lgob.push(list(b"trfl", vec![leaf(b"trfd", trfd(self.matrix))]));
        kids.push(list(b"lgob", lgob));
        kids.extend(self.extra.iter().cloned());
        list(b"obj ", kids)
    }
}

fn spid() -> Vec<u8> {
    // A version-4-shaped GUID; the reader never looks at it.
    vec![0x5e, 0xed, 0, 0, 0, 0, 0, 0x40, 0x80, 0, 0, 0, 0, 0, 0, 1]
}

fn common(flags: u32, b: [i32; 4]) -> Vec<Chunk> {
    let mut obbx = bbox(b);
    obbx.extend(bbox(b));
    vec![leaf(b"flgs", u32le(flags)), leaf(b"spid", spid()), leaf(b"bbox", bbox(b)), leaf(b"obbx", obbx)]
}

fn union(children: &[Chunk]) -> [i32; 4] {
    let mut out: Option<[i32; 4]> = None;
    for c in children {
        let Chunk::List(_, kids) = c else { continue };
        let Some(Chunk::Leaf(_, b)) = kids.iter().find(|k| matches!(k, Chunk::Leaf(id, _) if id == b"bbox")) else { continue };
        let v: Vec<i32> = b.as_chunks::<4>().0.iter().map(|s| i32::from_le_bytes(*s)).collect();
        let [x0, y1, x1, y0] = [v[0], v[1], v[2], v[3]];
        out = Some(match out {
            None => [x0, y1, x1, y0],
            Some([a, b, c, d]) => [a.min(x0), b.max(y1), c.max(x1), d.min(y0)],
        });
    }
    out.unwrap_or([-2, 2, 2, -2])
}

/// A group of drawables (first child front-most).
pub fn group(children: Vec<Chunk>) -> Chunk {
    group_with(0x1000_0000, None, children)
}

fn group_with(flags: u32, spnd: Option<u32>, children: Vec<Chunk>) -> Chunk {
    let mut kids = Vec::new();
    if let Some(s) = spnd {
        kids.push(leaf(b"spnd", u32le(s)));
    }
    kids.extend(common(flags, union(&children)));
    kids.push(list(b"lgob", vec![leaf(b"loda", loda(0, &[(ids::ARG_SERIAL, u32le(1))])), list(b"trfl", vec![leaf(b"trfd", trfd(IDENTITY))])]));
    kids.extend(children);
    list(b"grp ", kids)
}

/// A link group (the generated part of an effect).
pub fn link_group(children: Vec<Chunk>) -> Chunk {
    let mut kids = common(0x1800_0001, union(&children));
    kids.push(list(b"lgob", vec![leaf(b"loda", loda(15, &[(ids::ARG_SERIAL, u32le(1))]))]));
    kids.extend(children);
    list(b"lnkg", kids)
}

/// PowerClip contents: a `doc/clpt` group keyed by `spnd`.
pub fn clip_group(key: u32, children: Vec<Chunk>) -> Chunk {
    group_with(0x1080_0800, Some(key), children)
}

/// A layer. `kind` is the loda objType (0 art, 11 grid, 12 guides, 17 desktop).
pub fn layer(kind: u32, name: Option<&str>, guid: u8, master: bool, children: Vec<Chunk>) -> Chunk {
    let mut args = vec![(ids::ARG_LAYER_GUID, vec![guid; 16]), (ids::ARG_LAYER_COLOR, color_block(2, [0, 0, 0, 100]))];
    if let Some(n) = name {
        args.push((ids::ARG_NAME, utf16z(n)));
    }
    let flags = 0x9800_0040 | if master { 0x0001_0000 } else { 0 };
    let mut kids = vec![leaf(b"flgs", u32le(flags)), leaf(b"spid", spid()), list(b"lgob", vec![leaf(b"loda", loda(kind, &args))])];
    kids.extend(children);
    list(b"layr", kids)
}

/// A page (`master`: the master page).
pub fn page(master: bool, layers: Vec<Chunk>) -> Chunk {
    let flags = 0x9000_0040 | if master { 0x0001_0000 } else { 0 };
    let mut kids = common(flags, [-2, 2, 2, -2]);
    kids.push(list(b"gobj", layers));
    list(b"page", kids)
}

/// The paint tables every synthetic document carries by default: fill 1 is a uniform blue and
/// outline 1 draws nothing (the ids [`Obj`] uses unless told otherwise).
pub fn default_tables() -> Vec<Chunk> {
    vec![filt(vec![fild_uniform(1, &rgb(51, 102, 153))]), otlt(vec![outl_none(1)])]
}

/// The `doc ` list: page size, PowerClip contents and the default paint tables.
pub fn doc(width: i32, height: i32, clip_groups: Vec<Chunk>) -> Chunk {
    doc_with(width, height, clip_groups, default_tables())
}

/// The `doc ` list with these tables (`filt`, `otlt`, `stlt`, `fntt`…) instead of the defaults.
pub fn doc_with(width: i32, height: i32, clip_groups: Vec<Chunk>, tables: Vec<Chunk>) -> Chunk {
    let mut kids = vec![leaf(b"mcfg", mcfg(width, height))];
    kids.extend(tables);
    kids.push(list(b"clpt", clip_groups));
    list(b"doc ", kids)
}

/// A whole document: version records, `doc ` and pages.
pub fn document(version: u16, doc: Chunk, pages: Vec<Chunk>) -> Parts {
    document_with(version, Vec::new(), doc, pages)
}

/// A whole document with these `bmp ` records in a root `bmpt` list.
pub fn document_with(version: u16, bitmaps: Vec<Vec<u8>>, doc: Chunk, pages: Vec<Chunk>) -> Parts {
    let mut fver = version.to_le_bytes().to_vec();
    fver.extend([1, 0, 0, 0]);
    fver.extend((version / 100).to_le_bytes());
    let mut kids = vec![leaf(b"fver", fver), leaf(b"vrsn", version.to_le_bytes().to_vec())];
    if !bitmaps.is_empty() {
        kids.push(bmpt(bitmaps));
    }
    kids.push(doc);
    kids.extend(pages);
    Parts::split(b"CDRT", &kids)
}

/// A master page with a guides layer holding one vertical guideline, an empty desktop layer and
/// an empty grid layer (GUIDs 1, 2, 3).
pub fn master_page() -> Chunk {
    page(
        true,
        vec![
            layer(ids::GUIDES_LAYER, Some("Guides"), 1, true, vec![Obj::guideline(0).chunk()]),
            layer(ids::DESKTOP_LAYER, Some("Desktop"), 2, true, vec![]),
            layer(ids::GRID_LAYER, Some("Grid"), 3, true, vec![]),
        ],
    )
}

/// A document page holding the master mirrors and these art layers (first = front-most).
pub fn art_page(art_layers: Vec<Chunk>) -> Chunk {
    let mut layers = vec![layer(ids::GUIDES_LAYER, None, 1, true, vec![]), layer(ids::DESKTOP_LAYER, None, 2, true, vec![])];
    layers.extend(art_layers);
    layers.push(layer(ids::GRID_LAYER, None, 3, true, vec![]));
    page(false, layers)
}

/// A4 landscape in world units.
pub const PAGE: (i32, i32) = (2_970_000, 2_100_000);

/// The palette GUID of the sample, as written in `color/docPalette.xml`.
pub const SAMPLE_PALETTE: &str = "01234567-89ab-cdef-0123-456789abcdef";
/// [`SAMPLE_PALETTE`] as colour blocks store it (first three groups little-endian).
pub const SAMPLE_PALETTE_BYTES: [u8; 16] = [0x67, 0x45, 0x23, 0x01, 0xab, 0x89, 0xef, 0xcd, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
/// 12 pt in world units.
pub const SIZE_12PT: u32 = 42_333;

/// The paint, style and font tables of [`sample_parts`]: fills 1 (uniform blue), 2 (linear red to
/// black), 3 (radial grey ramp, used as a transparency), 4 (a named palette orange) and 5 (none);
/// outlines 1 (none) and 2 (black, 0.5 mm, round corners); one text style (id 1) on font 1.
pub fn sample_tables() -> Vec<Chunk> {
    let linear =
        Fountain { kind: 1, stops: vec![stop(rgb(255, 0, 0), 0), stop(color_block(3, [0, 0, 0, 255]), 100)], centre: (0.0, 0.0), matrix: IDENTITY };
    let ramp = Fountain { kind: 2, stops: vec![stop(gray(255), 0), stop(gray(0), 100)], centre: (0.0, 0.0), matrix: IDENTITY };
    let fills = filt(vec![
        fild_uniform(1, &rgb(51, 102, 153)),
        fild_fountain(2, &linear),
        fild_fountain(3, &ramp),
        fild_uniform(4, &color_block_in(3, [0, 128, 255, 0], SAMPLE_PALETTE_BYTES)),
        fild_none(5),
    ]);
    let outlines = otlt(vec![outl_none(1), outl(2, &Pen { join: 1, width: 5_000, ..Pen::default() })]);
    let styles = StyleTable {
        fills: vec![(1, 1)],
        outlines: vec![(2, 1)],
        fonts: vec![(3, [1, 1, 1], [SIZE_12PT; 3], [64; 3])],
        aligns: vec![(4, 0)],
        intervals: vec![(5, 0)],
        styles: vec![(1, [1, 2, 3, 4, 5, 0, 0])],
    };
    vec![fills, outlines, leaf(b"stlt", stlt(&styles)), fntt(vec![font(1, 64, "Synth Sans", "SynthSans-Regular")])]
}

/// The 4 × 4 picture of [`sample_parts`] (bitmap id 1): a red to blue ramp with a mask whose left
/// column is transparent.
pub fn sample_bitmap() -> Vec<u8> {
    let px: Vec<[u8; 3]> = (0..16u8).map(|i| [255 - 17 * i, 8 * i, 17 * i]).collect();
    let mask: Vec<u8> = (0..16).map(|i| if i % 4 == 0 { 0 } else { 255 }).collect();
    bmp(1, 4, 4, &Pixels::Rgb(px), Some(&mask))
}

/// A small document with every object kind the reader places: a rounded rectangle (linear
/// fountain, outline), an ellipse (a named palette colour), a curve with a Bézier segment (a
/// radial transparency ramp), a group, a link group (50 % uniform transparency), a PowerClip with
/// two clipped objects, artistic text with its story, a bitmap object with a masked picture and a
/// placeholder. Version 2600, A4 landscape, with a thumbnail, a font table and a document palette.
pub fn sample_parts() -> Parts {
    let triangle = Obj::polygon(&[(0, 0), (200_000, 0), (100_000, 150_000)]).at(translate(-500_000.0, 200_000.0));
    let wave = Obj::curve(&[(0, 0, MOVE), (100_000, 200_000, CONTROL), (300_000, 200_000, CONTROL), (400_000, 0, CURVE_END), (0, 0, LINE | CLOSE)])
        .at(translate(100_000.0, -400_000.0))
        .transparent(transparency(0.0, 100.0, 0, 3));
    let story =
        Story::new(b"Synth \x80 1", style_json("Synth Sans", SIZE_12PT, 64, "RGB255,USER,51,102,153,100,00000000-0000-0000-0000-000000000000", 0));
    let layer1 = vec![
        Obj::rectangle(500_000.0, -300_000.0, 0.25).at(translate(-1_400_000.0, 1_000_000.0)).fill(2).outline(2).chunk(),
        Obj::ellipse(400_000, 200_000).at([0.0, -1.0, 600_000.0, 1.0, 0.0, 300_000.0]).fill(4).chunk(),
        wave.chunk(),
        group(vec![triangle.chunk(), Obj::polygon(&[(0, 0), (50_000, 0), (50_000, 50_000)]).fill(5).chunk()]),
        Obj::ellipse(100_000, 100_000).at(translate(800_000.0, -800_000.0)).arg(40080, u32le(1)).chunk(),
        link_group(vec![Obj::ellipse(120_000, 120_000).at(translate(790_000.0, -810_000.0)).transparent(transparency(50.0, 50.0, 0, 1)).chunk()]),
        Obj::polygon(&[(0, 0), (300_000, 0), (300_000, 300_000), (0, 300_000)]).at(translate(-200_000.0, -900_000.0)).powerclip(5).chunk(),
        Obj::text().at(translate(-1_000_000.0, -700_000.0)).story(txsm(&Story { matrix: translate(-1_000_000.0, -700_000.0), ..story })).chunk(),
        Obj::bitmap([0, 254_000, 254_000, 0], 4, 4, 1).at(translate(1_000_000.0, 500_000.0)).chunk(),
        group(vec![Obj::placeholder().chunk()]),
    ];
    let contents = clip_group(
        5,
        vec![
            Obj::ellipse(250_000, 250_000).at(translate(-150_000.0, -850_000.0)).chunk(),
            Obj::polygon(&[(0, 0), (400_000, 0), (400_000, 100_000)]).at(translate(-250_000.0, -800_000.0)).chunk(),
        ],
    );
    let mut parts = document_with(
        2600,
        vec![sample_bitmap()],
        doc_with(PAGE.0, PAGE.1, vec![contents], sample_tables()),
        vec![master_page(), art_page(vec![layer(0, Some("Layer 1"), 4, false, layer1)])],
    );
    parts.thumbnail = Some(png(16, 9, [200, 210, 220]));
    parts.extra.push(("font/fontTable.dat".into(), font_table(&[(1, 64, "Synth Sans", "SynthSans-Regular")])));
    parts.extra.push((
        "color/docPalette.xml".into(),
        doc_palette(SAMPLE_PALETTE, "Synth palette", &[("CMYK", "Synth orange", &[0.0, 128.0 / 255.0, 1.0, 0.0])]),
    ));
    parts
}

/// [`sample_parts`] as a ZIP document with deflated entries.
pub fn sample() -> Vec<u8> {
    sample_parts().zip(Method::Deflate)
}
