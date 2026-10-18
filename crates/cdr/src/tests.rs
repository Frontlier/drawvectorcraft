//! Unit tests on synthetic files; `tests/real_files.rs` covers user-owned documents.

use crate::model::*;
use crate::synth::{self, Chunk, Method, Obj, ids, leaf, list};
use crate::{Error, Limits, is_cdr, preview, read, version};

const K: f64 = POINTS_PER_UNIT;
const W: f64 = synth::PAGE.0 as f64;
const H: f64 = synth::PAGE.1 as f64;

/// World units → page points (y down, origin at the page's top-left corner).
fn pt(x: f64, y: f64) -> Point {
    Point::new((x + W / 2.0) * K, (H / 2.0 - y) * K)
}

fn close(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6
}

/// A document with one art layer holding `children` (first = front-most) and these PowerClip groups.
fn doc_with(children: Vec<Chunk>, clip_groups: Vec<Chunk>) -> Vec<u8> {
    let page = synth::art_page(vec![synth::layer(0, Some("Art"), 4, false, children)]);
    synth::document(2600, synth::doc(synth::PAGE.0, synth::PAGE.1, clip_groups), vec![synth::master_page(), page]).zip(Method::Stored)
}

fn art(d: &Document) -> &[Node] {
    let layers = &d.pages[0].layers;
    assert_eq!(layers.len(), 1, "{layers:?}");
    &layers[0].nodes
}

fn path_of(n: &Node) -> &Path {
    match &n.kind {
        Kind::Shape { path, .. } => path,
        other => panic!("not a shape: {other:?}"),
    }
}

fn has_warning(d: &Document, text: &str) -> bool {
    d.warnings.iter().any(|w| w.contains(text))
}

/// Offset of a ZIP entry's local header.
fn local_header(zip: &[u8], name: &str) -> usize {
    (0..zip.len() - 30).find(|&p| zip[p..p + 4] == [0x50, 0x4b, 3, 4] && zip.get(p + 30..p + 30 + name.len()) == Some(name.as_bytes())).unwrap()
}

/// Offset of a ZIP entry's central-directory record.
fn central_record(zip: &[u8], name: &str) -> usize {
    (0..zip.len() - 46).find(|&p| zip[p..p + 4] == [0x50, 0x4b, 1, 2] && zip.get(p + 46..p + 46 + name.len()) == Some(name.as_bytes())).unwrap()
}

fn end_record(zip: &[u8]) -> usize {
    zip.len() - 22
}

mod detection {
    use super::*;

    #[test]
    fn the_sample_is_recognised_from_a_4k_head_and_empty_input_is_not() {
        let b = synth::sample();
        assert!(is_cdr(&b));
        assert!(is_cdr(&b[..b.len().min(4096)]));
        assert!(is_cdr(&b[..120]));
        assert!(!is_cdr(&[]));
        assert!(!is_cdr(b"PK"));
        assert!(!is_cdr(b"RIFF"));
    }

    #[test]
    fn arbitrary_zips_and_riffs_are_not_cdr() {
        assert!(!is_cdr(&synth::zip(&[("a.txt", b"hello", Method::Stored)])));
        assert!(!is_cdr(&synth::zip(&[("mimetype", b"application/epub+zip", Method::Stored)])));
        // The mimetype must be stored, as CorelDRAW writes it, so the head shows it.
        assert!(!is_cdr(&synth::zip(&[("mimetype", synth::MIMETYPE.as_bytes(), Method::Deflate)])));
        assert!(!is_cdr(&synth::zip(&[("content.dat", b"x", Method::Stored), ("content/root.dat", b"x", Method::Stored)])));
        assert!(!is_cdr(&synth::riff_inline(b"WAVE", &[leaf(b"fmt ", vec![0; 16])])));
        let plain = synth::zip(&[("a.txt", b"hello", Method::Stored)]);
        assert!(read(&plain, Limits::default()).is_err());
        assert!(preview(&plain).is_err());
    }

    #[test]
    fn every_corel_zip_signature_is_recognised() {
        assert!(is_cdr(&synth::zip(&[("mimetype", synth::MIMETYPE.as_bytes(), Method::Stored)])));
        assert!(is_cdr(&synth::zip(&[("content/root.dat", b"RIFF", Method::Deflate)])));
        assert!(is_cdr(&synth::zip(&[("content/riffData.cdr", b"RIFF", Method::Deflate)])));
    }

    #[test]
    fn older_layouts_are_recognised_but_refused() {
        let bare = synth::riff_inline(b"CDRD", &[leaf(b"vrsn", 1300u16.to_le_bytes().to_vec())]);
        assert!(is_cdr(&bare));
        assert!(matches!(read(&bare, Limits::default()), Err(Error::Unsupported(_))));
        assert!(matches!(version(&bare), Err(Error::Unsupported(_))));
        assert!(matches!(preview(&bare), Err(Error::Unsupported(_))));

        let older = synth::zip(&[("mimetype", synth::MIMETYPE.as_bytes(), Method::Stored), ("content/riffData.cdr", &bare, Method::Deflate)]);
        assert!(is_cdr(&older));
        assert!(matches!(read(&older, Limits::default()), Err(Error::Unsupported(m)) if m.contains("riffData")));

        // A root.dat whose leaves hold their data inline is an older layout too, not a misread.
        let lgob = list(b"lgob", vec![leaf(b"loda", synth::loda(3, &[(30, synth::path(&[(0, 0, synth::MOVE), (1, 1, synth::LINE)]))]))]);
        let inline = synth::riff_inline(b"CDRT", &[leaf(b"vrsn", 2600u16.to_le_bytes().to_vec()), list(b"obj ", vec![lgob])]);
        let zip = synth::zip(&[("mimetype", synth::MIMETYPE.as_bytes(), Method::Stored), ("content/root.dat", &inline, Method::Deflate)]);
        assert!(matches!(read(&zip, Limits::default()), Err(Error::Unsupported(m)) if m.contains("inline")));
    }

    #[test]
    fn error_messages_name_the_format() {
        assert_eq!(Error::Malformed("x").to_string(), "damaged CorelDRAW file: x");
        assert_eq!(Error::Unsupported("x").to_string(), "unsupported CorelDRAW file: x");
        assert_eq!(Error::Limit("x").to_string(), "CorelDRAW file exceeds a safety limit: x");
    }
}

mod zip {
    use super::*;

    fn stored_sample() -> Vec<u8> {
        synth::sample_parts().zip(Method::Stored)
    }

    #[test]
    fn checksums_are_verified() {
        let mut b = stored_sample();
        let at = local_header(&b, "content/root.dat") + 30 + "content/root.dat".len() + 20;
        b[at] ^= 0x01;
        assert_eq!(read(&b, Limits::default()).unwrap_err(), Error::Malformed("ZIP entry checksum mismatch"));
    }

    #[test]
    fn declared_sizes_over_the_limits_are_refused_before_allocating() {
        let b = synth::sample();
        assert_eq!(read(&b, Limits { max_entry: 64, ..Limits::default() }).unwrap_err(), Error::Limit("ZIP entry too large"));
        assert_eq!(read(&b, Limits { max_total: 200, ..Limits::default() }).unwrap_err(), Error::Limit("document too large"));
        // A bomb: a tiny deflate stream claiming 4 GiB.
        let mut bomb = b.clone();
        let cd = central_record(&bomb, "content/root.dat");
        bomb[cd + 24..cd + 28].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(read(&bomb, Limits::default()).unwrap_err(), Error::Limit("ZIP entry too large"));
        // A declared size smaller than the stream: decoding stops one byte past it.
        let mut short = b;
        let cd = central_record(&short, "content/root.dat");
        let size = u32::from_le_bytes(short[cd + 24..cd + 28].try_into().unwrap());
        short[cd + 24..cd + 28].copy_from_slice(&(size - 1).to_le_bytes());
        assert_eq!(read(&short, Limits::default()).unwrap_err(), Error::Malformed("ZIP entry size mismatch"));
    }

    #[test]
    fn encryption_zip64_and_unknown_methods_are_refused() {
        let b = stored_sample();
        let cd = central_record(&b, "content/root.dat");
        let mut encrypted = b.clone();
        encrypted[cd + 8] |= 1;
        assert!(matches!(read(&encrypted, Limits::default()), Err(Error::Unsupported(m)) if m.contains("encrypted")));
        let mut zip64 = b.clone();
        zip64[cd + 20..cd + 24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(read(&zip64, Limits::default()), Err(Error::Unsupported(m)) if m.contains("ZIP64")));
        let mut count = b.clone();
        let end = end_record(&count);
        count[end + 8..end + 12].copy_from_slice(&[0xFF; 4]);
        assert!(matches!(read(&count, Limits::default()), Err(Error::Unsupported(m)) if m.contains("ZIP64")));
        let mut locator = b.clone();
        let end = end_record(&locator);
        let mut record = 0x0706_4b50u32.to_le_bytes().to_vec();
        record.extend([0; 16]);
        locator.splice(end..end, record);
        assert!(matches!(read(&locator, Limits::default()), Err(Error::Unsupported(m)) if m.contains("ZIP64")));
        let mut method = b;
        method[cd + 10] = 12;
        assert!(matches!(read(&method, Limits::default()), Err(Error::Unsupported(m)) if m.contains("method")));
    }

    #[test]
    fn duplicate_names_offsets_and_counts_are_checked() {
        let dup = synth::zip(&[("mimetype", synth::MIMETYPE.as_bytes(), Method::Stored), ("a", b"1", Method::Stored), ("a", b"2", Method::Stored)]);
        assert_eq!(crate::zip::Zip::open(&dup, Limits::default()).unwrap_err(), Error::Malformed("duplicate ZIP entry names"));
        let b = stored_sample();
        let mut offset = b.clone();
        let cd = central_record(&offset, "content/root.dat");
        offset[cd + 42..cd + 46].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        assert_eq!(read(&offset, Limits::default()).unwrap_err(), Error::Malformed("ZIP entry outside the file"));
        let mut directory = b.clone();
        let end = end_record(&directory);
        directory[end + 16..end + 20].copy_from_slice(&0x7FFF_FFF0u32.to_le_bytes());
        assert!(matches!(read(&directory, Limits::default()), Err(Error::Malformed(_))));
        let mut many = b;
        let end = end_record(&many);
        many[end + 8..end + 12].copy_from_slice(&[0x11, 0x27, 0x11, 0x27]);
        assert_eq!(read(&many, Limits::default()).unwrap_err(), Error::Limit("too many ZIP entries"));
        assert!(read(b"PK\x03\x04", Limits::default()).is_err());
    }

    #[test]
    fn data_file_names_are_bare_and_references_stay_inside_their_file() {
        let mut parts = synth::sample_parts();
        parts.data[0].0 = "../evil.dat".into();
        assert_eq!(read(&parts.zip(Method::Stored), Limits::default()).unwrap_err(), Error::Malformed("bad data file name"));
        // Truncated data files: every reference past the end fails, so the document is refused.
        let mut parts = synth::sample_parts();
        for (_, data) in &mut parts.data {
            data.truncate(data.len() / 4);
        }
        assert!(read(&parts.zip(Method::Stored), Limits::default()).is_err());
    }
}

mod structure {
    use super::*;

    #[test]
    fn the_sample_has_every_kind_in_reverse_file_order() {
        let d = read(&synth::sample(), Limits::default()).unwrap();
        assert_eq!(d.version, 2600);
        assert_eq!(version(&synth::sample()), Ok(2600));
        assert_eq!(d.pages.len(), 1);
        let p = &d.pages[0];
        assert_eq!(p.name, "Page 1");
        assert!((p.width - 841.889_763_779_527_6).abs() < 1e-9 && (p.height - 595.275_590_551_181_1).abs() < 1e-9);
        assert_eq!(p.layers[0].name, "Layer 1");
        assert!(p.layers[0].visible && !p.layers[0].locked && p.layers[0].printable && !p.layers[0].master);
        assert_eq!(p.layers[0].color, Some(Color::Cmyk { c: 0.0, m: 0.0, y: 0.0, k: 1.0 }));
        // The master guides layer's vertical guideline at x = 0 is the page's centre line.
        assert_eq!(p.guides.len(), 1);
        assert!((p.guides[0].a.x - W / 2.0 * K).abs() < 1e-9 && (p.guides[0].b.x - W / 2.0 * K).abs() < 1e-9);
        let kinds: Vec<&str> = art(&d)
            .iter()
            .map(|n| match &n.kind {
                Kind::Group { .. } => "group",
                Kind::Shape { .. } => "shape",
                Kind::Text(_) => "text",
                Kind::Image(_) => "image",
                Kind::PowerClip { .. } => "powerclip",
            })
            .collect();
        // File order (front-most first): rectangle, ellipse, wave, group, ellipse, link group,
        // PowerClip, text, bitmap, placeholder group (dropped: nothing to draw).
        assert_eq!(kinds, ["image", "text", "powerclip", "group", "shape", "group", "shape", "shape", "shape"]);
        // Everything in the sample is decoded: no warning at all.
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    #[test]
    fn objects_are_placed_by_their_own_transform_with_y_down() {
        let tri = Obj::polygon(&[(0, 0), (100_000, 0), (0, 50_000)]).at(synth::translate(300_000.0, -200_000.0));
        // A group transform is never composed in: it is already baked into its children.
        let mut grouped = synth::group(vec![tri.chunk()]);
        if let Chunk::List(_, kids) = &mut grouped {
            for k in kids.iter_mut() {
                if let Chunk::List(id, lgob) = k
                    && id == b"lgob"
                {
                    lgob[1] = list(b"trfl", vec![leaf(b"trfd", synth::trfd(synth::translate(9e6, 9e6)))]);
                }
            }
        }
        let d = read(&doc_with(vec![grouped], vec![]), Limits::default()).unwrap();
        let Kind::Group { children } = &art(&d)[0].kind else { panic!() };
        let s = &path_of(&children[0]).subpaths[0];
        assert!(close(s.start, pt(300_000.0, -200_000.0)));
        let Segment::Line(p) = s.segments[1] else { panic!() };
        assert!(close(p, pt(300_000.0, -150_000.0)));
        assert!(p.y < s.start.y, "up in CorelDRAW is up on the page");
        assert!(s.closed);
        // A rotation by 90°: x' = −y, y' = x.
        let rotated = Obj::polygon(&[(0, 0), (100_000, 0), (0, 50_000)]).at([0.0, -1.0, 0.0, 1.0, 0.0, 0.0]);
        let d = read(&doc_with(vec![rotated.chunk()], vec![]), Limits::default()).unwrap();
        let Segment::Line(p) = path_of(&art(&d)[0]).subpaths[0].segments[0] else { panic!() };
        assert!(close(p, pt(0.0, 100_000.0)));
    }

    #[test]
    fn stored_bounds_are_reported_in_page_space() {
        let tri = Obj::polygon(&[(0, 0), (100_000, 0), (0, 50_000)]).at(synth::translate(300_000.0, -200_000.0));
        let d = read(&doc_with(vec![tri.chunk()], vec![]), Limits::default()).unwrap();
        let n = &art(&d)[0];
        let stored = n.bounds.unwrap();
        let drawn = path_of(n).bounds().unwrap();
        assert!((stored.x0 - drawn.x0).abs() < 1e-6 && (stored.y1 - drawn.y1).abs() < 1e-6, "{stored:?} {drawn:?}");
        assert!(close(Point::new(stored.x0, stored.y0), pt(300_000.0, -150_000.0)));
    }

    #[test]
    fn rectangles_with_and_without_corner_radii() {
        let plain = Obj::rectangle(400_000.0, -200_000.0, 0.0).at(synth::translate(-100_000.0, 100_000.0));
        let round = Obj::rectangle(400_000.0, -200_000.0, 0.5).at(synth::translate(-100_000.0, 100_000.0));
        let d = read(&doc_with(vec![round.chunk(), plain.chunk()], vec![]), Limits::default()).unwrap();
        let (p, r) = (path_of(&art(&d)[0]), path_of(&art(&d)[1]));
        let s = &p.subpaths[0];
        assert!(close(s.start, pt(-100_000.0, 100_000.0)) && s.closed);
        let corners: Vec<Point> = s.segments.iter().map(Segment::end).collect();
        assert!(
            close(corners[0], pt(300_000.0, 100_000.0))
                && close(corners[1], pt(300_000.0, -100_000.0))
                && close(corners[2], pt(-100_000.0, -100_000.0))
        );
        // Radius = fraction × min(|w|, |h|) / 2 = 50 000: four quarter circles, same bounds.
        let cubics = r.subpaths[0].segments.iter().filter(|g| matches!(g, Segment::Cubic(..))).count();
        assert_eq!(cubics, 4);
        assert!(close(r.subpaths[0].start, pt(-100_000.0, 50_000.0)));
        let (pb, rb) = (p.bounds().unwrap(), r.bounds().unwrap());
        assert!((pb.x0 - rb.x0).abs() < 1e-6 && (pb.y0 - rb.y0).abs() < 1e-6 && (pb.x1 - rb.x1).abs() < 1e-6 && (pb.y1 - rb.y1).abs() < 1e-6);
    }

    #[test]
    fn ellipses_arcs_and_pies() {
        let full = Obj::ellipse(200_000, -100_000).at(synth::translate(50_000.0, 0.0));
        let pie = Obj::arc(200_000, 200_000, 0, 90_000_000, 0);
        let arc = Obj::arc(200_000, 200_000, 0, 90_000_000, 2);
        let d = read(&doc_with(vec![arc.chunk(), pie.chunk(), full.chunk()], vec![]), Limits::default()).unwrap();
        let nodes = art(&d);
        let e = path_of(&nodes[0]);
        assert_eq!(e.subpaths[0].segments.len(), 4);
        let b = e.bounds().unwrap();
        let (a, c) = (pt(50_000.0, 0.0), pt(250_000.0, -100_000.0));
        assert!((b.x0 - a.x).abs() < 1e-6 && (b.y0 - a.y).abs() < 1e-6 && (b.x1 - c.x).abs() < 1e-6 && (b.y1 - c.y).abs() < 1e-6, "{b:?}");
        // A quarter pie from 0° to 90°, counter-clockwise in CorelDRAW space, back to the centre.
        let pie = &path_of(&nodes[1]).subpaths[0];
        assert!(close(pie.start, pt(200_000.0, 100_000.0)) && pie.closed);
        assert!(close(pie.segments[0].end(), pt(100_000.0, 200_000.0)));
        assert!(close(pie.segments[1].end(), pt(100_000.0, 100_000.0)));
        let arc = &path_of(&nodes[2]).subpaths[0];
        assert!(!arc.closed && arc.segments.len() == 1);
        assert!(has_warning(&d, "elliptical arcs and pies (unverified) (2×)"), "{:?}", d.warnings);
    }

    #[test]
    fn curves_with_beziers_open_and_several_subpaths() {
        use synth::{CLOSE, CONTROL, CURVE_END, LINE, MOVE};
        let c = Obj::curve(&[
            (0, 0, MOVE),
            (10_000, 20_000, CONTROL),
            (30_000, 20_000, CONTROL),
            (40_000, 0, CURVE_END),
            (0, 0, LINE | CLOSE),
            (100_000, 0, MOVE),
            (150_000, 0, LINE),
        ]);
        let d = read(&doc_with(vec![c.chunk()], vec![]), Limits::default()).unwrap();
        let p = path_of(&art(&d)[0]);
        assert_eq!(p.subpaths.len(), 2);
        let Segment::Cubic(c1, c2, end) = p.subpaths[0].segments[0] else { panic!() };
        assert!(close(c1, pt(10_000.0, 20_000.0)) && close(c2, pt(30_000.0, 20_000.0)) && close(end, pt(40_000.0, 0.0)));
        assert!(p.subpaths[0].closed && !p.subpaths[1].closed);
        // Tight bounds: the cubic's top is at 15 000, below its control points.
        let b = p.bounds().unwrap();
        assert!((b.y0 - pt(0.0, 15_000.0).y).abs() < 1e-6, "{b:?}");
        assert_eq!(p.point_count(), 7);
        assert!(matches!(art(&d)[0].kind, Kind::Shape { even_odd: true, .. }));
    }

    #[test]
    fn a_damaged_object_is_skipped_but_most_damaged_is_unsupported() {
        use synth::{CONTROL, LINE, MOVE};
        let bad = || Obj::curve(&[(0, 0, MOVE), (1, 1, CONTROL), (2, 2, LINE)]).chunk();
        let good = || Obj::polygon(&[(0, 0), (10, 0), (0, 10)]).chunk();
        let d = read(&doc_with(vec![good(), bad(), good()], vec![]), Limits::default()).unwrap();
        assert_eq!(art(&d).len(), 2);
        assert!(has_warning(&d, "objects that could not be read: a malformed Bézier segment"), "{:?}", d.warnings);
        // A finite transform can still throw an object to infinity: it is skipped, not passed on.
        let far = Obj::polygon(&[(0, 0), (1_000_000, 0), (0, 1)]).at([1e300, 0.0, 0.0, 0.0, 1.0, 0.0]).chunk();
        let text = Obj::text().at([1e300, 0.0, 0.0, 0.0, 1.0, 0.0]).chunk();
        let d = read(&doc_with(vec![good(), far, text, good(), good()], vec![]), Limits::default()).unwrap();
        assert_eq!(art(&d).len(), 3);
        assert!(has_warning(&d, "objects that could not be read: an object placed beyond any usable range (2×)"), "{:?}", d.warnings);
        let wrong_length = Obj::rectangle(1.0, 1.0, 0.0).arg(ids::ARG_GEOMETRY, vec![0; 40]).chunk();
        assert_eq!(
            read(&doc_with(vec![good(), bad(), wrong_length], vec![]), Limits::default()).unwrap_err(),
            Error::Unsupported("most objects could not be decoded")
        );
    }

    #[test]
    fn z_order_is_reversed_in_layers_groups_and_pages() {
        let a = Obj::polygon(&[(0, 0), (1, 0), (0, 1)]).named("front");
        let b = Obj::polygon(&[(0, 0), (1, 0), (0, 1)]).named("back");
        let names = |nodes: &[Node]| nodes.iter().map(|n| n.name.clone().unwrap_or_default()).collect::<Vec<_>>();
        let d = read(&doc_with(vec![a.chunk(), b.chunk(), synth::group(vec![a.chunk(), b.chunk()])], vec![]), Limits::default()).unwrap();
        assert_eq!(names(art(&d)), ["", "back", "front"]);
        let Kind::Group { children } = &art(&d)[0].kind else { panic!() };
        assert_eq!(names(children), ["back", "front"]);
        // Two art layers: the first in the file is the front-most, so it comes last.
        let page = synth::art_page(vec![
            synth::layer(0, Some("Top"), 4, false, vec![a.chunk()]),
            synth::layer(0, Some("Bottom"), 5, false, vec![b.chunk()]),
        ]);
        let file = synth::document(2600, synth::doc(synth::PAGE.0, synth::PAGE.1, vec![]), vec![synth::master_page(), page]).zip(Method::Stored);
        let d = read(&file, Limits::default()).unwrap();
        assert_eq!(d.pages[0].layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["Bottom", "Top"]);
    }

    #[test]
    fn powerclips_link_their_contents_by_key() {
        let frame = Obj::polygon(&[(0, 0), (100_000, 0), (100_000, 100_000), (0, 100_000)]).powerclip(5);
        let contents = synth::clip_group(5, vec![Obj::ellipse(10, 10).named("front").chunk(), Obj::ellipse(10, 10).named("back").chunk()]);
        let unused = synth::clip_group(7, vec![Obj::ellipse(10, 10).named("unused").chunk()]);
        let missing = Obj::polygon(&[(0, 0), (1, 0), (0, 1)]).powerclip(9);
        let d = read(&doc_with(vec![frame.chunk(), missing.chunk()], vec![contents, unused]), Limits::default()).unwrap();
        let nodes = art(&d);
        assert_eq!(nodes.len(), 2);
        assert!(matches!(nodes[0].kind, Kind::Shape { .. }));
        let Kind::PowerClip { frame, contents, contents_locked } = &nodes[1].kind else { panic!("{:?}", nodes[1]) };
        assert!(matches!(frame.kind, Kind::Shape { .. }) && *contents_locked);
        assert_eq!(contents.iter().map(|n| n.name.as_deref().unwrap_or("")).collect::<Vec<_>>(), ["back", "front"]);
        assert!(nodes[1].bounds.is_some());
        assert!(has_warning(&d, "a PowerClip whose contents are missing"), "{:?}", d.warnings);
    }

    #[test]
    fn powerclip_cycles_and_shared_contents_terminate() {
        let frame = |key| Obj::polygon(&[(0, 0), (10, 0), (0, 10)]).powerclip(key).chunk();
        // 5 holds a frame of 5; 6 and 8 hold frames of each other.
        let groups = vec![synth::clip_group(5, vec![frame(5)]), synth::clip_group(6, vec![frame(8)]), synth::clip_group(8, vec![frame(6)])];
        let d = read(&doc_with(vec![frame(5), frame(6), frame(6)], groups), Limits::default()).unwrap();
        assert_eq!(art(&d).len(), 3);
        assert!(has_warning(&d, "a PowerClip that contains itself"), "{:?}", d.warnings);
        assert!(has_warning(&d, "PowerClip contents shared by several frames"), "{:?}", d.warnings);
        let Kind::PowerClip { contents, .. } = &art(&d)[2].kind else { panic!() };
        assert!(matches!(contents[0].kind, Kind::Shape { .. }), "the self-reference is drawn as its frame only");
    }

    #[test]
    fn nesting_deeper_than_128_levels_is_cut_with_a_warning() {
        let mut node = Obj::polygon(&[(0, 0), (1, 0), (0, 1)]).chunk();
        for _ in 0..200 {
            node = synth::group(vec![node]);
        }
        let d = read(&doc_with(vec![node, Obj::polygon(&[(0, 0), (1, 0), (0, 1)]).chunk()], vec![]), Limits::default()).unwrap();
        assert!(has_warning(&d, "nested deeper than 128 levels"), "{:?}", d.warnings);
        fn depth(n: &Node) -> usize {
            match &n.kind {
                Kind::Group { children } => 1 + children.iter().map(depth).max().unwrap_or(0),
                _ => 1,
            }
        }
        assert!(art(&d).iter().map(depth).max().unwrap() <= 128);
        // Chunk nesting beyond the tree's own limit is refused outright.
        let mut deep = leaf(b"flgs", vec![0; 4]);
        for _ in 0..300 {
            deep = list(b"grp ", vec![deep]);
        }
        let parts = synth::document(2600, synth::doc(1, 1, vec![deep]), vec![]);
        assert_eq!(read(&parts.zip(Method::Stored), Limits::default()).unwrap_err(), Error::Limit("chunks nested too deeply"));
    }

    #[test]
    fn the_deepest_documents_fit_a_small_stack() {
        // The web build has a 1 MiB stack and aborts on overflow: the deepest group nesting and
        // the longest PowerClip chain the limits allow must fit well inside half of that.
        let run = || {
            let mut node = Obj::polygon(&[(0, 0), (1, 0), (0, 1)]).chunk();
            for _ in 0..130 {
                node = synth::group(vec![node]);
            }
            let d = read(&doc_with(vec![node], vec![]), Limits::default()).unwrap();
            assert!(has_warning(&d, "nested deeper than 128 levels"), "{:?}", d.warnings);
            let frame = |key| Obj::polygon(&[(0, 0), (10, 0), (0, 10)]).powerclip(key).chunk();
            let groups = (1..=130).map(|k| synth::clip_group(k, vec![frame(k + 1)])).collect();
            let d = read(&doc_with(vec![frame(1)], groups), Limits::default()).unwrap();
            assert!(has_warning(&d, "nested deeper than 128 levels"), "{:?}", d.warnings);
        };
        std::thread::Builder::new().stack_size(512 << 10).spawn(run).unwrap().join().unwrap();
    }

    #[test]
    fn documents_without_pages_or_art_layers_are_unsupported() {
        let none = synth::document(2600, synth::doc(synth::PAGE.0, synth::PAGE.1, vec![]), vec![synth::master_page()]);
        assert_eq!(read(&none.zip(Method::Stored), Limits::default()).unwrap_err(), Error::Unsupported("the document has no pages"));
        let guides_only =
            synth::document(2600, synth::doc(synth::PAGE.0, synth::PAGE.1, vec![]), vec![synth::master_page(), synth::art_page(vec![])]);
        assert_eq!(read(&guides_only.zip(Method::Stored), Limits::default()).unwrap_err(), Error::Unsupported("no art layer found"));
        let no_doc = synth::Parts::split(b"CDRT", &[leaf(b"vrsn", 2600u16.to_le_bytes().to_vec())]);
        assert!(read(&no_doc.zip(Method::Stored), Limits::default()).is_err());
    }

    #[test]
    fn master_art_layers_repeat_on_every_page() {
        let master = synth::page(true, vec![synth::layer(0, Some("Logo"), 9, true, vec![Obj::ellipse(10, 10).chunk()])]);
        let page = || synth::page(false, vec![synth::layer(0, Some("Art"), 4, false, vec![Obj::polygon(&[(0, 0), (1, 0), (0, 1)]).chunk()])]);
        let file = synth::document(2600, synth::doc(synth::PAGE.0, synth::PAGE.1, vec![]), vec![master, page(), page()]).zip(Method::Stored);
        let d = read(&file, Limits::default()).unwrap();
        assert_eq!(d.pages.len(), 2);
        for p in &d.pages {
            assert_eq!(p.layers.iter().map(|l| (l.name.as_str(), l.master)).collect::<Vec<_>>(), [("Art", false), ("Logo", true)]);
        }
        assert!(has_warning(&d, "master page layers (repeated on every page) (2×)"), "{:?}", d.warnings);
        assert_eq!(d.pages[1].name, "Page 2");
    }

    #[test]
    fn unknown_versions_and_missing_page_sizes_warn() {
        let file = synth::document(
            2700,
            list(b"doc ", vec![]),
            vec![synth::art_page(vec![synth::layer(0, None, 4, false, vec![Obj::ellipse(10, 10).chunk()])])],
        );
        let d = read(&file.zip(Method::Deflate), Limits::default()).unwrap();
        assert_eq!(d.version, 2700);
        assert!(has_warning(&d, "format version 2700 has not been verified"), "{:?}", d.warnings);
        assert!(has_warning(&d, "no page size (A4 assumed)"), "{:?}", d.warnings);
        assert!((d.pages[0].width - 595.275_590_551_181_1).abs() < 1e-9);
        assert_eq!(d.pages[0].layers[0].name, "Layer 1");
    }

    #[test]
    fn warnings_are_deduplicated_with_counts() {
        let tri = || Obj::polygon(&[(0, 0), (1, 0), (0, 1)]).fill(99).chunk();
        let d = read(&doc_with(vec![tri(), tri(), tri()], vec![]), Limits::default()).unwrap();
        assert!(d.warnings.contains(&"objects whose fill record is missing (not filled) (3×)".to_string()), "{:?}", d.warnings);
        let unique: std::collections::BTreeSet<_> = d.warnings.iter().collect();
        assert_eq!(unique.len(), d.warnings.len());
    }

    #[test]
    fn placeholders_and_guidelines_are_not_art() {
        let d = read(&doc_with(vec![Obj::placeholder().chunk(), Obj::guideline(5).chunk(), Obj::ellipse(10, 10).chunk()], vec![]), Limits::default())
            .unwrap();
        assert_eq!(art(&d).len(), 1);
        assert!(has_warning(&d, "guidelines among the art (not imported)"), "{:?}", d.warnings);
    }

    #[test]
    fn text_is_placed_at_its_baseline_with_upright_glyphs() {
        let d = read(&doc_with(vec![Obj::text().at(synth::translate(-100_000.0, 50_000.0)).chunk()], vec![]), Limits::default()).unwrap();
        let Kind::Text(t) = &art(&d)[0].kind else { panic!() };
        assert!(t.runs.is_empty());
        let m = t.transform;
        assert!(close(m.apply(Point::new(0.0, 0.0)), pt(-100_000.0, 50_000.0)));
        // One point right and one point down in text space are one point right and down on the page.
        assert!(close(m.apply(Point::new(1.0, 0.0)), Point::new(pt(-100_000.0, 50_000.0).x + 1.0, pt(-100_000.0, 50_000.0).y)));
        assert!(close(m.apply(Point::new(0.0, 1.0)), Point::new(pt(-100_000.0, 50_000.0).x, pt(-100_000.0, 50_000.0).y + 1.0)));
        assert!(m.determinant() > 0.0);
    }

    #[test]
    fn bitmaps_without_their_pixels_keep_their_frame() {
        let d = read(&doc_with(vec![Obj::bitmap([0, 254_000, 508_000, 0], 72, 36, 1).chunk()], vec![]), Limits::default()).unwrap();
        let Kind::Shape { path, fill, .. } = &art(&d)[0].kind else { panic!() };
        assert_eq!(*fill, Paint::Unsupported("bitmap"));
        let b = path.bounds().unwrap();
        assert!((b.width() - 144.0).abs() < 1e-6 && (b.height() - 72.0).abs() < 1e-6, "{b:?}");
        assert!(has_warning(&d, "bitmaps whose pixels are missing (shown as their outline)"), "{:?}", d.warnings);
    }

    #[test]
    fn affine_composition_order() {
        let a = Affine::translate(1.0, 2.0);
        let b = Affine::scale_non_uniform(10.0, -1.0);
        let p = a.then(b).apply(Point::new(1.0, 1.0));
        assert!(close(p, Point::new(20.0, -3.0)));
        assert!(Affine::IDENTITY.is_finite() && !Affine([f64::NAN, 0.0, 0.0, 1.0, 0.0, 0.0]).is_finite());
        assert!((Affine::scale_non_uniform(4.0, 9.0).scale() - 6.0).abs() < 1e-12);
    }
}

mod robustness {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn every_truncation_is_handled() {
        for method in [Method::Stored, Method::Deflate] {
            let b = synth::sample_parts().zip(method);
            assert!(read(&b, Limits::default()).is_ok());
            for end in 0..b.len() {
                let head = &b[..end];
                let _ = is_cdr(head);
                let _ = version(head);
                // The end record is the last thing in a ZIP: no prefix is a whole document.
                assert!(read(head, Limits::default()).is_err(), "{end}");
                assert!(preview(head).is_err(), "{end}");
            }
        }
        let root = synth::sample_parts().root;
        for end in 0..root.len() {
            let _ = crate::riff::Tree::parse(&root[..end]);
        }
    }

    #[test]
    fn every_truncated_stream_or_data_file_is_handled() {
        let parts = synth::sample_parts();
        for end in (0..parts.root.len()).step_by(7) {
            let mut p = parts.clone();
            p.root.truncate(end);
            assert!(read(&p.zip(Method::Stored), Limits::default()).is_err(), "{end}");
        }
        for f in 0..parts.data.len() {
            for end in (0..parts.data[f].1.len()).step_by(13) {
                let mut p = parts.clone();
                p.data[f].1.truncate(end);
                let _ = read(&p.zip(Method::Stored), Limits::default());
            }
        }
    }

    fn mutate(b: &mut [u8], edits: &[(usize, u8)]) {
        if b.is_empty() {
            return;
        }
        let n = b.len();
        for (i, v) in edits {
            b[i % n] = *v;
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn arbitrary_input_never_panics(b in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = is_cdr(&b);
            let _ = read(&b, Limits::default());
            let _ = preview(&b);
            let _ = version(&b);
            let _ = crate::riff::Tree::parse(&b);
            let mut riff = b"RIFF\x00\x10\x00\x00CDRT".to_vec();
            riff.extend(&b);
            let _ = crate::riff::Tree::parse(&riff);
            let _ = read(&synth::zip(&[("content/root.dat", &riff, Method::Stored)]), Limits::default());
        }

        #[test]
        fn mutations_never_panic(edits in prop::collection::vec((any::<usize>(), any::<u8>()), 1..32)) {
            for method in [Method::Stored, Method::Deflate] {
                let mut b = synth::sample_parts().zip(method);
                mutate(&mut b, &edits);
                let _ = is_cdr(&b);
                let _ = read(&b, Limits::default());
                let _ = preview(&b);
            }
        }

        #[test]
        fn mutated_streams_and_data_files_never_panic(edits in prop::collection::vec((any::<usize>(), any::<u8>()), 1..24), target in 0usize..8) {
            // Mutations inside the entries, re-zipped with valid checksums, reach the chunk tree,
            // the references, every record decoder and the side entries (fonts, palette).
            let mut parts = synth::sample_parts();
            let (files, extra) = (parts.data.len(), parts.extra.len());
            match target % (files + extra + 1) {
                0 => mutate(&mut parts.root, &edits),
                i if i <= files => mutate(&mut parts.data[i - 1].1, &edits),
                i => mutate(&mut parts.extra[i - 1 - files].1, &edits),
            }
            let _ = read(&parts.zip(Method::Stored), Limits::default());
        }
    }
}

mod previews {
    use super::*;

    fn with_previews(thumbnail: Option<&[u8]>, page: Option<&[u8]>) -> Vec<u8> {
        let mut entries: Vec<(&str, &[u8], Method)> = vec![("mimetype", synth::MIMETYPE.as_bytes(), Method::Stored)];
        if let Some(t) = thumbnail {
            entries.push(("previews/thumbnail.png", t, Method::Stored));
        }
        if let Some(p) = page {
            entries.push(("previews/page1.png", p, Method::Deflate));
        }
        synth::zip(&entries)
    }

    #[test]
    fn the_thumbnail_comes_first_and_the_page_picture_stands_in() {
        let p = preview(&synth::sample()).unwrap();
        assert_eq!((p.width, p.height), (16, 9));
        assert_eq!(p.png, synth::png(16, 9, [200, 210, 220]));
        let thumb = synth::png(4, 3, [0, 0, 0]);
        let page = synth::png(8, 6, [255, 255, 255]);
        assert_eq!(preview(&with_previews(Some(&thumb), Some(&page))).unwrap().width, 4);
        assert_eq!(preview(&with_previews(None, Some(&page))).unwrap().width, 8);
        let mut damaged = thumb.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert_eq!(preview(&with_previews(Some(&damaged), Some(&page))).unwrap().width, 8);
        assert_eq!(preview(&with_previews(Some(&damaged), None)).unwrap_err(), Error::Malformed("PNG chunk checksum"));
        assert_eq!(preview(&with_previews(None, None)).unwrap_err(), Error::Unsupported("no embedded preview"));
    }

    #[test]
    fn every_truncated_or_oversized_png_is_rejected() {
        let png = synth::png(4, 3, [1, 2, 3]);
        for end in 0..png.len() {
            assert!(preview(&with_previews(Some(&png[..end]), None)).is_err(), "{end}");
        }
        let mut wide = png.clone();
        wide[16..20].copy_from_slice(&5000u32.to_be_bytes());
        let crc = crc32fast::hash(&wide[12..29]);
        wide[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(preview(&with_previews(Some(&wide), None)).unwrap_err(), Error::Limit("preview dimensions must be 1..4096"));
        let mut trailing = png;
        trailing.push(0);
        assert_eq!(preview(&with_previews(Some(&trailing), None)).unwrap_err(), Error::Malformed("PNG end or trailing data"));
    }

    #[test]
    fn compressed_metadata_and_animation_are_refused() {
        for kind in [b"zTXt", b"iTXt", b"iCCP", b"acTL"] {
            let mut png = synth::png(2, 2, [9, 9, 9]);
            let mut extra = vec![0; 4];
            extra.extend(kind);
            extra.extend(crc32fast::hash(kind).to_be_bytes());
            png.splice(33..33, extra);
            assert!(matches!(preview(&with_previews(Some(&png), None)), Err(Error::Unsupported(_))));
        }
    }
}

/// A document whose `doc ` list holds these tables, with one art layer of `children`.
fn doc_tables(children: Vec<Chunk>, tables: Vec<Chunk>) -> synth::Parts {
    let page = synth::art_page(vec![synth::layer(0, Some("Art"), 4, false, children)]);
    synth::document(2600, synth::doc_with(synth::PAGE.0, synth::PAGE.1, vec![], tables), vec![synth::master_page(), page])
}

fn read_parts(p: &synth::Parts) -> Document {
    read(&p.zip(Method::Stored), Limits::default()).unwrap()
}

fn fill_of(n: &Node) -> &Paint {
    match &n.kind {
        Kind::Shape { fill, .. } => fill,
        other => panic!("not a shape: {other:?}"),
    }
}

fn outline_of(n: &Node) -> Option<&Outline> {
    match &n.kind {
        Kind::Shape { outline, .. } => outline.as_ref(),
        other => panic!("not a shape: {other:?}"),
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9 * (1.0 + a.abs().max(b.abs()))
}

/// A 200 000 × 100 000 unit rectangle drawn as a curve, at `(x, y)`.
fn box_at(x: f64, y: f64) -> Obj {
    Obj::polygon(&[(0, 0), (200_000, 0), (200_000, 100_000), (0, 100_000)]).at(synth::translate(x, y))
}

mod paint {
    use super::*;
    use crate::synth::{Fountain, Pen, stop};

    fn tables(fills: Vec<Vec<u8>>, outlines: Vec<Vec<u8>>) -> Vec<Chunk> {
        let mut outlines = outlines;
        if outlines.is_empty() {
            outlines.push(synth::outl_none(1));
        }
        vec![synth::filt(fills), synth::otlt(outlines)]
    }

    #[test]
    fn uniform_fills_in_every_verified_colour_model() {
        let fills = vec![
            synth::fild_uniform(1, &synth::rgb(10, 20, 30)),
            synth::fild_uniform(2, &synth::color_block(2, [10, 20, 30, 40])),
            synth::fild_uniform(3, &synth::color_block(3, [51, 102, 153, 204])),
            synth::fild_uniform(4, &synth::color_block(17, [0, 0, 0, 255])),
            synth::fild_uniform(5, &synth::gray(51)),
            synth::fild_none(6),
        ];
        let objects = (1..=6).rev().map(|i| box_at(0.0, 0.0).fill(i).chunk()).collect();
        let d = read_parts(&doc_tables(objects, tables(fills, vec![])));
        let fills: Vec<&Paint> = art(&d).iter().map(fill_of).collect();
        let solid = |c: Color| Paint::Solid(c);
        assert_eq!(*fills[0], solid(Color::Rgb { r: 10.0 / 255.0, g: 20.0 / 255.0, b: 30.0 / 255.0 }));
        assert_eq!(*fills[1], solid(Color::Cmyk { c: 0.1, m: 0.2, y: 0.3, k: 0.4 }));
        assert_eq!(*fills[2], solid(Color::Cmyk { c: 0.2, m: 0.4, y: 0.6, k: 0.8 }));
        assert_eq!(*fills[3], solid(Color::Cmyk { c: 0.0, m: 0.0, y: 0.0, k: 1.0 }));
        assert_eq!(*fills[4], solid(Color::Gray { v: 0.2 }));
        assert_eq!(*fills[5], Paint::None);
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    #[test]
    fn linear_fountains_run_across_the_object_box_in_page_space() {
        let mut red = stop(synth::rgb(255, 0, 0), 0);
        red.midpoint = 30;
        red.alpha = 128;
        let f = Fountain { kind: 1, stops: vec![red, stop(synth::rgb(0, 0, 255), 100)], centre: (0.0, 0.0), matrix: synth::IDENTITY };
        let d = read_parts(&doc_tables(vec![box_at(100_000.0, -50_000.0).fill(1).chunk()], tables(vec![synth::fild_fountain(1, &f)], vec![])));
        let Paint::Gradient(g) = fill_of(&art(&d)[0]) else { panic!("{:?}", art(&d)[0]) };
        assert_eq!(g.kind, GradientKind::Linear);
        // Offset 0 on the box's left edge, offset 1 on its right edge, at mid height.
        assert!(close(g.transform.apply(Point::new(0.0, 0.0)), pt(100_000.0, 0.0)), "{:?}", g.transform);
        assert!(close(g.transform.apply(Point::new(1.0, 0.0)), pt(300_000.0, 0.0)));
        assert_eq!(g.stops.len(), 2);
        assert_eq!(g.stops[0].color, Color::Rgb { r: 1.0, g: 0.0, b: 0.0 });
        assert!(near(g.stops[0].opacity, 128.0 / 255.0) && near(g.stops[0].midpoint, 0.3) && near(g.stops[1].offset, 1.0));
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    #[test]
    fn radial_fountains_use_their_centre_offset_and_matrix() {
        let f = Fountain {
            kind: 2,
            stops: vec![stop(synth::gray(255), 0), stop(synth::gray(0), 100)],
            centre: (0.25, 0.0),
            matrix: [0.5, 0.0, 0.0, 0.0, 0.5, 0.0],
        };
        let rotated = box_at(0.0, 0.0).at([0.0, -1.0, 0.0, 1.0, 0.0, 0.0]).fill(1);
        let d = read_parts(&doc_tables(vec![rotated.chunk()], tables(vec![synth::fild_fountain(1, &f)], vec![])));
        let Paint::Gradient(g) = fill_of(&art(&d)[0]) else { panic!() };
        assert_eq!(g.kind, GradientKind::Radial);
        // Local centre (100 000 + 0.25·2·100 000, 50 000), radius half the box width, then the 90° turn.
        assert!(close(g.transform.apply(Point::new(0.0, 0.0)), pt(-50_000.0, 150_000.0)), "{:?}", g.transform);
        assert!(close(g.transform.apply(Point::new(1.0, 0.0)), pt(-50_000.0, 200_000.0)));
    }

    #[test]
    fn odd_fountains_are_repaired_or_reported() {
        let out_of_order = Fountain {
            kind: 3,
            stops: vec![stop(synth::gray(0), 0), stop(synth::gray(100), 70), stop(synth::gray(200), 40), stop(synth::gray(255), 100)],
            centre: (0.0, 0.0),
            matrix: synth::IDENTITY,
        };
        let single = Fountain { kind: 1, stops: vec![stop(synth::rgb(1, 2, 3), 0)], centre: (0.0, 0.0), matrix: synth::IDENTITY };
        let singular = Fountain { kind: 1, stops: vec![stop(synth::gray(0), 0), stop(synth::gray(255), 100)], centre: (0.0, 0.0), matrix: [0.0; 6] };
        let fills = vec![synth::fild_fountain(1, &out_of_order), synth::fild_fountain(2, &single), synth::fild_fountain(3, &singular)];
        let objects = vec![box_at(0.0, 0.0).fill(3).chunk(), box_at(0.0, 0.0).fill(2).chunk(), box_at(0.0, 0.0).fill(1).chunk()];
        let d = read_parts(&doc_tables(objects, tables(fills, vec![])));
        let Paint::Gradient(g) = fill_of(&art(&d)[0]) else { panic!() };
        assert_eq!(g.kind, GradientKind::Conical);
        let offsets: Vec<f64> = g.stops.iter().map(|s| s.offset).collect();
        assert_eq!(offsets, [0.0, 0.7, 0.7, 1.0]);
        assert_eq!(*fill_of(&art(&d)[1]), Paint::Solid(Color::Rgb { r: 1.0 / 255.0, g: 2.0 / 255.0, b: 3.0 / 255.0 }));
        // A singular placement falls back to the object box itself.
        let Paint::Gradient(g) = fill_of(&art(&d)[2]) else { panic!() };
        assert!(close(g.transform.apply(Point::new(1.0, 0.0)), pt(200_000.0, 50_000.0)));
        for w in [
            "conical fountain fills (unverified)",
            "fountain colours out of order (reordered)",
            "fountain fills with a single colour (filled with it)",
            "fountain fill placements that could not be used (centred on the object)",
        ] {
            assert!(has_warning(&d, w), "{w}: {:?}", d.warnings);
        }
    }

    #[test]
    fn unsupported_fill_types_and_damaged_records_are_reported() {
        let mut damaged = synth::fild_uniform(3, &synth::rgb(1, 2, 3));
        damaged.truncate(40);
        let fills = vec![synth::fild_type(1, 7, &[1, 2, 3, 4]), damaged];
        let objects = vec![box_at(0.0, 0.0).fill(3).chunk(), box_at(0.0, 0.0).fill(1).chunk(), box_at(0.0, 0.0).fill(42).chunk()];
        let d = read_parts(&doc_tables(objects, tables(fills, vec![])));
        assert_eq!(*fill_of(&art(&d)[0]), Paint::None);
        assert!(matches!(fill_of(&art(&d)[1]), Paint::Unsupported(_)));
        assert_eq!(*fill_of(&art(&d)[2]), Paint::None);
        assert!(has_warning(&d, "(fill type 7; not imported)"), "{:?}", d.warnings);
        assert!(has_warning(&d, "fills that could not be decoded"), "{:?}", d.warnings);
        assert!(has_warning(&d, "objects whose fill record is missing"), "{:?}", d.warnings);
    }

    #[test]
    fn outlines_carry_width_corners_miter_and_order() {
        let pens = vec![
            synth::outl(1, &Pen { join: 1, width: 10_000, flags: 0x12, ..Pen::default() }),
            synth::outl(2, &Pen { flags: 0x22, width: 10_000, miter_micro_degrees: 11_478_340, color: synth::rgb(0, 128, 0), ..Pen::default() }),
            synth::outl(3, &Pen { flags: 0x01, ..Pen::default() }),
            synth::outl(4, &Pen { width: 0, ..Pen::default() }),
            synth::outl(5, &Pen { join: 9, stretch: 50, extras: [1; 38], ..Pen::default() }),
        ];
        let objects = vec![
            box_at(0.0, 0.0).outline(5).chunk(),
            box_at(0.0, 0.0).outline(4).chunk(),
            box_at(0.0, 0.0).outline(3).chunk(),
            box_at(0.0, 0.0).at([2.0, 0.0, 0.0, 0.0, 3.0, 0.0]).outline(2).chunk(),
            box_at(0.0, 0.0).outline(1).chunk(),
        ];
        let d = read_parts(&doc_tables(objects, tables(vec![synth::fild_none(1)], pens)));
        let nodes = art(&d);
        let o = outline_of(&nodes[0]).unwrap();
        assert!(near(o.width, 10_000.0 * K) && o.join == Join::Round && o.behind_fill && !o.scale_with_object && o.cap == Cap::Butt);
        assert!(near(o.miter_limit, 1.0 / (2.5f64.to_radians()).sin()), "{}", o.miter_limit);
        assert_eq!(o.paint, Paint::Solid(Color::Cmyk { c: 0.0, m: 0.0, y: 0.0, k: 1.0 }));
        assert!(matches!(nodes[0].kind, Kind::Shape { outline_behind: true, .. }));
        // Scaling with the object: its largest stretch (3) applies; the miter angle is a ratio of 10.
        let o = outline_of(&nodes[1]).unwrap();
        assert!(near(o.width, 30_000.0 * K) && o.scale_with_object && (o.miter_limit - 10.0).abs() < 1e-5, "{o:?}");
        assert_eq!(o.paint, Paint::Solid(Color::Rgb { r: 0.0, g: 128.0 / 255.0, b: 0.0 }));
        assert!(outline_of(&nodes[2]).is_none() && outline_of(&nodes[3]).is_none());
        assert_eq!(outline_of(&nodes[4]).unwrap().join, Join::Miter);
        for w in ["outline dashes, arrowheads or line caps", "calligraphic outlines", "outline corners of an unknown kind"] {
            assert!(has_warning(&d, w), "{w}: {:?}", d.warnings);
        }
    }

    #[test]
    fn transparency_becomes_opacity_masks_and_blend_modes() {
        let ramp =
            Fountain { kind: 1, stops: vec![stop(synth::gray(255), 0), stop(synth::gray(51), 100)], centre: (0.0, 0.0), matrix: synth::IDENTITY };
        let fills = vec![synth::fild_uniform(1, &synth::rgb(0, 0, 0)), synth::fild_fountain(2, &ramp)];
        let objects = vec![
            box_at(0.0, 0.0).transparent(synth::transparency(0.0, 0.0, 29, 1)).chunk(),
            box_at(0.0, 0.0).transparent(synth::transparency(0.0, 0.0, 0, 9)).lgob(b"ttil", synth::tile(synth::translate(1.0, 0.0))).chunk(),
            box_at(0.0, 0.0).transparent(synth::transparency(0.0, 100.0, 30, 2)).chunk(),
            box_at(0.0, 0.0).transparent(synth::transparency(25.0, 25.0, 0, 1)).chunk(),
        ];
        let d = read_parts(&doc_tables(objects, tables(fills, vec![])));
        let nodes = art(&d);
        // The amount is the start value; the black of the fill is not.
        assert!(near(nodes[0].opacity, 0.75) && nodes[0].opacity_mask.is_none() && nodes[0].blend == Blend::Normal);
        let m = nodes[1].opacity_mask.as_ref().unwrap();
        assert_eq!(nodes[1].blend, Blend::Overlay);
        assert!(near(nodes[1].opacity, 1.0) && m.kind == GradientKind::Linear);
        assert!(near(m.stops[0].opacity, 1.0) && near(m.stops[1].opacity, 0.2), "{m:?}");
        assert!(close(m.transform.apply(Point::new(1.0, 0.0)), pt(200_000.0, 50_000.0)));
        assert!(near(nodes[2].opacity, 1.0) && nodes[2].opacity_mask.is_none());
        assert_eq!(nodes[3].blend, Blend::Normal);
        for w in [
            "transparency merge mode 30 (drawn as overlay, approximated)",
            "transparency merge mode 29 (not known; drawn as normal)",
            "transparency whose ramp record is missing",
            "transparency tile offsets (ignored)",
        ] {
            assert!(has_warning(&d, w), "{w}: {:?}", d.warnings);
        }
    }

    #[test]
    fn palette_colours_keep_their_swatch_names() {
        let named = synth::color_block_in(3, [0, 128, 255, 0], synth::SAMPLE_PALETTE_BYTES);
        let unnamed = synth::color_block_in(3, [0, 128, 255, 0], [0; 16]);
        let other = synth::color_block_in(5, [9, 9, 9, 0], synth::SAMPLE_PALETTE_BYTES);
        let fills = vec![synth::fild_uniform(1, &named), synth::fild_uniform(2, &unnamed), synth::fild_uniform(3, &other)];
        let objects = vec![box_at(0.0, 0.0).fill(3).chunk(), box_at(0.0, 0.0).fill(2).chunk(), box_at(0.0, 0.0).fill(1).chunk()];
        let mut parts = doc_tables(objects, tables(fills, vec![]));
        let entries: [(&str, &str, &[f64]); 2] = [("RGB", "Grey &amp; co", &[0.5, 0.5, 0.5]), ("CMYK", "Orange", &[0.0, 128.0 / 255.0, 1.0, 0.0])];
        parts.extra.push(("color/docPalette.xml".into(), synth::doc_palette(synth::SAMPLE_PALETTE, "Brand", &entries)));
        let d = read_parts(&parts);
        let base = Color::Cmyk { c: 0.0, m: 128.0 / 255.0, y: 1.0, k: 0.0 };
        let Paint::Solid(Color::Named { name, palette, spot, tint, base: b }) = fill_of(&art(&d)[0]) else { panic!("{:?}", art(&d)[0]) };
        assert_eq!((name.as_str(), palette.as_deref(), *spot, *tint, &**b), ("Orange", Some("Brand"), false, 1.0, &base));
        assert_eq!(*fill_of(&art(&d)[1]), Paint::Solid(base));
        // The palette has no entry of that value: the colour stays unnamed.
        assert!(matches!(fill_of(&art(&d)[2]), Paint::Solid(Color::Rgb { .. })));
    }

    #[test]
    fn unknown_colour_models_fall_back_to_the_style_cache_or_black() {
        let fills = vec![synth::fild_uniform(1, &synth::color_block(42, [1, 2, 3, 4]))];
        let json = br#"{"fill":{"type":"1","primaryColor":"RGB255,USER,255,0,51,100,00000000-0000-0000-0000-000000000000"}}"#;
        let mut cache = 1u32.to_le_bytes().to_vec();
        cache.extend((json.len() as u32).to_le_bytes());
        cache.extend(json);
        let objects = vec![box_at(0.0, 0.0).arg(201, cache).chunk(), box_at(0.0, 0.0).chunk()];
        let d = read_parts(&doc_tables(objects, tables(fills, vec![])));
        assert_eq!(*fill_of(&art(&d)[0]), Paint::Solid(Color::Rgb { r: 0.0, g: 0.0, b: 0.0 }));
        assert_eq!(*fill_of(&art(&d)[1]), Paint::Solid(Color::Rgb { r: 1.0, g: 0.0, b: 0.2 }));
        assert!(has_warning(&d, "(taken from the object's style cache)") && has_warning(&d, "(drawn black)"), "{:?}", d.warnings);
    }

    #[test]
    fn object_styles_supply_missing_paint() {
        let style = synth::StyleTable { fills: vec![(1, 2)], outlines: vec![(2, 1)], styles: vec![(7, [1, 2, 0, 0, 0, 0, 0])], ..Default::default() };
        let mut t = tables(vec![synth::fild_none(1), synth::fild_uniform(2, &synth::gray(0))], vec![]);
        t.push(leaf(b"stlt", synth::stlt(&style)));
        let styled = box_at(0.0, 0.0).without(ids::ARG_FILL).without(ids::ARG_OUTLINE).arg(12000, synth::u32le(7));
        let plain = box_at(0.0, 0.0).without(ids::ARG_FILL);
        let d = read_parts(&doc_tables(vec![plain.chunk(), styled.chunk()], t));
        assert_eq!(*fill_of(&art(&d)[0]), Paint::Solid(Color::Gray { v: 0.0 }));
        assert_eq!(*fill_of(&art(&d)[1]), Paint::None);
        assert!(has_warning(&d, "paint taken from an object style (unverified)"), "{:?}", d.warnings);
    }

    #[test]
    fn style_tables_of_another_layout_are_ignored() {
        let mut t = tables(vec![synth::fild_none(1)], vec![]);
        let mut table = synth::stlt(&synth::StyleTable { styles: vec![(1, [0; 7])], ..Default::default() });
        table.push(0);
        t.push(leaf(b"stlt", table));
        let d = read_parts(&doc_tables(vec![box_at(0.0, 0.0).chunk()], t));
        assert!(has_warning(&d, "the style table (an unfamiliar layout"), "{:?}", d.warnings);
    }

    #[test]
    fn mesh_fills_take_their_average_node_colour() {
        let mut blob = vec![7u8; 13];
        for v in [150u32, 1, 2] {
            blob.extend(v.to_le_bytes());
        }
        for c in [synth::rgb(0, 0, 0), synth::rgb(255, 255, 255)] {
            blob.extend(43u32.to_le_bytes());
            blob.extend(c);
        }
        let mut mesh = box_at(0.0, 0.0).without(ids::ARG_FILL).arg(19150, blob);
        mesh.obj_type = 32;
        let mut empty = box_at(0.0, 0.0).without(ids::ARG_FILL).arg(19150, vec![0; 40]);
        empty.obj_type = 32;
        let d = read_parts(&doc_tables(vec![empty.chunk(), mesh.chunk()], synth::default_tables()));
        assert_eq!(*fill_of(&art(&d)[0]), Paint::Solid(Color::Rgb { r: 0.5, g: 0.5, b: 0.5 }));
        assert_eq!(*fill_of(&art(&d)[1]), Paint::None);
        assert!(has_warning(&d, "mesh fills (filled with their average colour)"), "{:?}", d.warnings);
        assert!(has_warning(&d, "mesh fills (not decoded; not filled)"), "{:?}", d.warnings);
    }

    #[test]
    fn the_winding_argument_selects_nonzero() {
        let objects = vec![box_at(0.0, 0.0).arg(14001, synth::u32le(1)).chunk(), box_at(0.0, 0.0).chunk()];
        let d = read_parts(&doc_tables(objects, synth::default_tables()));
        assert!(matches!(art(&d)[0].kind, Kind::Shape { even_odd: true, .. }));
        assert!(matches!(art(&d)[1].kind, Kind::Shape { even_odd: false, .. }));
    }

    #[test]
    fn the_sample_resolves_every_paint() {
        let d = read(&synth::sample(), Limits::default()).unwrap();
        let nodes = art(&d);
        // Bottom to top: bitmap, text, PowerClip, link group, ellipse, group, wave, ellipse, rectangle.
        let Paint::Gradient(g) = fill_of(&nodes[8]) else { panic!() };
        assert_eq!(g.stops[1].color, Color::Cmyk { c: 0.0, m: 0.0, y: 0.0, k: 1.0 });
        assert!(outline_of(&nodes[8]).is_some_and(|o| o.join == Join::Round && near(o.width, 5_000.0 * K)));
        assert!(matches!(fill_of(&nodes[7]), Paint::Solid(Color::Named { name, .. }) if name == "Synth orange"));
        assert!(nodes[6].opacity_mask.as_ref().is_some_and(|m| m.kind == GradientKind::Radial && near(m.stops[1].opacity, 0.0)));
        let Kind::Group { children } = &nodes[3].kind else { panic!() };
        assert!(near(children[0].opacity, 0.5));
        let Kind::Group { children } = &nodes[5].kind else { panic!() };
        assert_eq!(*fill_of(&children[0]), Paint::None);
    }
}

mod text {
    use super::*;
    use crate::synth::{Story, StyleTable};

    const FILL: &str = "RGB255,USER,51,102,153,100,00000000-0000-0000-0000-000000000000";

    fn tables_with(style: &StyleTable) -> Vec<Chunk> {
        let mut t = synth::default_tables();
        t.push(leaf(b"stlt", synth::stlt(style)));
        t.push(synth::fntt(vec![synth::font(1, 64, "Body Face", "BodyFace-Regular"), synth::font(2, 0x100, "Display Face", "DisplayFace-Medium")]));
        t
    }

    fn style_table(align: u32) -> StyleTable {
        StyleTable {
            fills: vec![(1, 1)],
            outlines: vec![(2, 1)],
            fonts: vec![(3, [1, 1, 1], [synth::SIZE_12PT; 3], [64; 3])],
            aligns: vec![(4, align)],
            intervals: vec![(5, 500_000)],
            styles: vec![(1, [1, 2, 3, 4, 5, 0, 0])],
        }
    }

    fn text_of(d: &Document) -> &Text {
        match &art(d)[0].kind {
            Kind::Text(t) => t,
            other => panic!("not text: {other:?}"),
        }
    }

    fn one(story: &Story, m: [f64; 6], style: &StyleTable) -> Document {
        let obj = Obj::text().at(m).story(synth::txsm(&Story { matrix: m, ..story.clone() }));
        read_parts(&doc_tables(vec![obj.chunk()], tables_with(style)))
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn runs_come_from_the_story_and_its_style() {
        let story = Story::new(b"Caf\xe9 \x80\x81!", synth::style_json("Display Face", 84_667, 0x100, FILL, 1_000_000));
        let d = one(&story, synth::translate(-100_000.0, 50_000.0), &style_table(0));
        let t = text_of(&d);
        assert_eq!(t.align, Align::Left);
        assert_eq!(t.runs.len(), 1);
        let r = &t.runs[0];
        assert_eq!(r.text, "Café €\u{FFFD}!");
        assert_eq!((r.family.as_str(), r.postscript_name.as_deref()), ("Display Face", Some("DisplayFace-Medium")));
        assert!((r.size_pt - 84_667.0 * K).abs() < 1e-9 && (r.size_pt - 24.0).abs() < 1e-3, "{}", r.size_pt);
        assert_eq!((r.weight, r.italic, r.underline), (500, false, false));
        assert!(near(r.tracking, 0.3));
        // The object's fill chain (arg 20 → fill 1) and no outline (outline 1 is off).
        assert_eq!(r.fill, Paint::Solid(Color::Rgb { r: 0.2, g: 0.4, b: 0.6 }));
        assert!(r.outline.is_none());
        assert!(close(t.transform.apply(Point::new(0.0, 0.0)), pt(-100_000.0, 50_000.0)));
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    #[test]
    fn a_uniform_scale_moves_into_the_size_and_glyphs_stay_upright() {
        let story = Story::new(b"Up", synth::style_json("Body Face", synth::SIZE_12PT, 64, FILL, 0));
        let (s, c) = (2.0 * 30f64.to_radians().sin(), 2.0 * 30f64.to_radians().cos());
        let d = one(&story, [c, -s, 10_000.0, s, c, 20_000.0], &style_table(0));
        let t = text_of(&d);
        assert!((t.runs[0].size_pt - 24.0).abs() < 1e-3, "{}", t.runs[0].size_pt);
        assert!((t.transform.determinant() - 1.0).abs() < 1e-9, "{:?}", t.transform);
        // Text space x runs 30° up from the page's x axis (page y points down).
        let a = t.transform.apply(Point::new(0.0, 0.0));
        let b = t.transform.apply(Point::new(1.0, 0.0));
        assert!(((a.y - b.y).atan2(b.x - a.x).to_degrees() - 30.0).abs() < 1e-6);
        assert!(close(a, pt(10_000.0, 20_000.0)));
    }

    #[test]
    fn character_styles_override_the_object_style() {
        let mut story = Story::new(b"Hi", synth::style_json("Body Face", synth::SIZE_12PT, 64, FILL, 0));
        story.char_style = concat!(
            r#"{"character":{"latin":{"size":"84667","italic":"1"},"#,
            r#""fill":{"type":"1","primaryColor":"CMYK,USER,0,100,0,0,100,00000000-0000-0000-0000-000000000000"}}}"#
        )
        .into();
        let d = one(&story, synth::IDENTITY, &style_table(0));
        let r = &text_of(&d).runs[0];
        assert!((r.size_pt - 24.0).abs() < 1e-3 && r.italic && r.family == "Body Face");
        assert_eq!(r.fill, Paint::Solid(Color::Cmyk { c: 0.0, m: 1.0, y: 0.0, k: 0.0 }));
    }

    #[test]
    fn the_style_table_stands_in_for_unreadable_json() {
        let story = Story::new(b"Hi", "{not json".into());
        let d = one(&story, synth::IDENTITY, &style_table(2));
        let r = &text_of(&d).runs[0];
        assert_eq!((r.family.as_str(), r.postscript_name.as_deref(), r.weight), ("Body Face", Some("BodyFace-Regular"), 400));
        assert!((r.size_pt - 12.0).abs() < 1e-3 && near(r.tracking, 0.15), "{r:?}");
        assert!(has_warning(&d, "text styles that could not be read"), "{:?}", d.warnings);
        assert!(has_warning(&d, "text alignments other than left"), "{:?}", d.warnings);
    }

    #[test]
    fn unverified_layouts_are_recovered_best_effort() {
        // Two bytes per character: read as UTF-16.
        let mut story = Story::new(&utf16("Ωmega"), synth::style_json("Body Face", 1, 64, FILL, 0));
        story.chars = Some(5);
        let d = one(&story, synth::IDENTITY, &style_table(0));
        assert_eq!(text_of(&d).runs[0].text, "Ωmega");
        assert!(has_warning(&d, "text stored in an unverified layout"), "{:?}", d.warnings);
        // Non-zero character records: the document's text index supplies the characters.
        let mut story = Story::new(b"xx", synth::style_json("Body Face", 1, 64, FILL, 0));
        story.records = Some(vec![1; 16]);
        let obj = Obj::text().story(synth::txsm(&story));
        let mut parts = doc_tables(vec![obj.chunk()], tables_with(&style_table(0)));
        let xml = concat!(
            r#"<x:xmpmeta><TextStreams><TextStream><TextRun break="para" lang="1033">A &amp; B</TextRun>"#,
            r#"<TextRun>C</TextRun></TextStream></TextStreams></x:xmpmeta>"#
        );
        parts.extra.push(("META-INF/textinfo.xml".into(), xml.as_bytes().to_vec()));
        let d = read_parts(&parts);
        assert_eq!(text_of(&d).runs[0].text, "A & B\nC");
        assert!(has_warning(&d, "several character styles"), "{:?}", d.warnings);
    }

    #[test]
    fn far_east_characters_use_their_own_slot() {
        let json = concat!(
            r#"{"character":{"latin":{"font":"Body Face","size":"42333","weight":"64"},"#,
            r#""farEast":{"font":"East Face","size":"42333","weight":"64"}}}"#
        );
        let mut story = Story::new(&utf16("ab 東京"), json.into());
        story.chars = Some(5);
        let d = one(&story, synth::IDENTITY, &style_table(0));
        let runs: Vec<(&str, &str)> = text_of(&d).runs.iter().map(|r| (r.text.as_str(), r.family.as_str())).collect();
        assert_eq!(runs, [("ab ", "Body Face"), ("東京", "East Face")]);
    }

    #[test]
    fn damaged_or_missing_stories_leave_an_empty_text() {
        let d = read_parts(&doc_tables(vec![Obj::text().chunk()], synth::default_tables()));
        assert!(text_of(&d).runs.is_empty() && has_warning(&d, "text objects without their characters"));
        let mut bytes = synth::txsm(&Story::new(b"Hello", synth::style_json("Body Face", 1, 64, FILL, 0)));
        bytes.truncate(bytes.len() - 12);
        let d = read_parts(&doc_tables(vec![Obj::text().story(bytes).chunk()], synth::default_tables()));
        assert!(text_of(&d).runs.is_empty(), "{:?}", text_of(&d));
        assert!(has_warning(&d, "text that could not be decoded: a truncated text story"), "{:?}", d.warnings);
    }

    #[test]
    fn weights_follow_the_bit_field() {
        let mut w = Warnings::default();
        assert_eq!([0x40, 0x100, 0x10000, 1 << 20].map(|b| crate::text::css_weight(b, &mut w)), [400, 500, 900, 900]);
        assert!(w.finish().is_empty());
        let mut w = Warnings::default();
        assert_eq!(crate::text::css_weight(0x41, &mut w), 400);
        assert_eq!(w.finish().len(), 1);
    }
}

mod bitmaps {
    use super::*;
    use crate::bitmap::decode_record;
    use crate::synth::Pixels;

    /// Offset of the image block's info header in a `bmp ` record built by `synth::bmp`.
    const INFO: usize = 0x28 + 14;

    fn doc_with_bitmap(obj: Obj, record: Vec<u8>, limits: Limits) -> Document {
        let page = synth::art_page(vec![synth::layer(0, Some("Art"), 4, false, vec![obj.chunk()])]);
        let parts = synth::document_with(2600, vec![record], synth::doc(synth::PAGE.0, synth::PAGE.1, vec![]), vec![synth::master_page(), page]);
        read(&parts.zip(Method::Stored), limits).unwrap()
    }

    #[test]
    fn pictures_decode_bottom_up_with_padding_and_masks() {
        // 3 × 2, 24-bit: rows of 9 bytes padded to 12 with garbage.
        let px = vec![[1, 2, 3], [4, 5, 6], [7, 8, 9], [10, 11, 12], [13, 14, 15], [16, 17, 18]];
        let (w, h, rgba) = decode_record(&synth::bmp(1, 3, 2, &Pixels::Rgb(px), Some(&[0, 50, 100, 150, 200, 255]))).unwrap();
        assert_eq!((w, h), (3, 2));
        assert_eq!(rgba, [1, 2, 3, 0, 4, 5, 6, 50, 7, 8, 9, 100, 10, 11, 12, 150, 13, 14, 15, 200, 16, 17, 18, 255]);
        // 8-bit indexed: B, G, R palette entries; an index past the palette is black.
        let palette = vec![[255, 0, 0], [0, 0, 255]];
        let (_, _, rgba) = decode_record(&synth::bmp(2, 2, 2, &Pixels::Indexed(palette, vec![0, 1, 1, 7]), None)).unwrap();
        assert_eq!(rgba, [255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn damaged_and_unknown_records_are_refused() {
        let good = synth::bmp(1, 2, 2, &Pixels::Rgb(vec![[0; 3]; 4]), None);
        assert!(decode_record(&good).is_ok());
        for end in 0..good.len() {
            assert!(decode_record(&good[..end]).is_err(), "{end}");
        }
        let mut model = good.clone();
        model[INFO..INFO + 4].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(decode_record(&model).unwrap_err(), "unsupported");
        let mut stride = good.clone();
        stride[INFO + 24..INFO + 28].copy_from_slice(&2u32.to_le_bytes());
        assert!(decode_record(&stride).is_err());
        let mut huge = good;
        huge[INFO + 8..INFO + 12].copy_from_slice(&70_000u32.to_le_bytes());
        assert!(decode_record(&huge).is_err());
    }

    #[test]
    fn bitmap_objects_become_placed_images() {
        let frame = [0, 254_000, 254_000, 0];
        let obj = Obj::bitmap(frame, 4, 4, 1).at(synth::translate(100_000.0, 0.0));
        let d = doc_with_bitmap(obj, synth::sample_bitmap(), Limits::default());
        let Kind::Image(im) = &art(&d)[0].kind else { panic!("{:?}", art(&d)[0]) };
        assert_eq!((im.width, im.height, im.rgba.len()), (4, 4, 64));
        assert!(im.clip.is_none());
        // Pixel (0, 0) is the frame's top-left corner, (4, 4) its bottom-right corner.
        assert!(close(im.transform.apply(Point::new(0.0, 0.0)), pt(100_000.0, 254_000.0)));
        assert!(close(im.transform.apply(Point::new(4.0, 4.0)), pt(354_000.0, 0.0)));
        assert_eq!(&im.rgba[..8], &[255, 0, 0, 0, 238, 8, 17, 255]);
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    #[test]
    fn objects_showing_one_picture_share_its_pixels_and_budget() {
        // Two objects name picture 1; a budget of one picture (16 pixels) decodes both.
        let a = Obj::bitmap([0, 254_000, 254_000, 0], 4, 4, 1);
        let b = Obj::bitmap([0, 254_000, 254_000, 0], 4, 4, 1).at(synth::translate(500_000.0, 0.0));
        let page = synth::art_page(vec![synth::layer(0, Some("Art"), 4, false, vec![a.chunk(), b.chunk()])]);
        let parts = synth::document_with(
            2600,
            vec![synth::sample_bitmap()],
            synth::doc(synth::PAGE.0, synth::PAGE.1, vec![]),
            vec![synth::master_page(), page],
        );
        let d = read(&parts.zip(Method::Stored), Limits { max_pixels: 16, ..Limits::default() }).unwrap();
        let [Kind::Image(first), Kind::Image(second)] = [&art(&d)[0].kind, &art(&d)[1].kind] else { panic!("{:?}", art(&d)) };
        assert!(std::sync::Arc::ptr_eq(&first.rgba, &second.rgba), "one buffer for one picture");
        assert_ne!(first.transform, second.transform);
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    #[test]
    fn cropped_bitmaps_keep_their_crop_outline() {
        let mut record = synth::bitmap_record([0, 254_000, 254_000, 0], 4, 4, 1);
        record[56..60].copy_from_slice(&1u32.to_le_bytes());
        let obj = Obj::bitmap([0, 254_000, 254_000, 0], 4, 4, 1).arg(ids::ARG_GEOMETRY, record);
        let d = doc_with_bitmap(obj, synth::sample_bitmap(), Limits::default());
        let Kind::Image(im) = &art(&d)[0].kind else { panic!() };
        let clip = im.clip.as_ref().unwrap().bounds().unwrap();
        assert!((clip.width() - 72.0).abs() < 1e-9 && (clip.height() - 72.0).abs() < 1e-9);
    }

    #[test]
    fn budgets_sizes_and_missing_pictures_degrade_to_the_outline() {
        let obj = || Obj::bitmap([0, 254_000, 254_000, 0], 4, 4, 1);
        let d = doc_with_bitmap(obj(), synth::sample_bitmap(), Limits { max_pixels: 10, ..Limits::default() });
        assert!(matches!(fill_of(&art(&d)[0]), Paint::Unsupported("bitmap")) && has_warning(&d, "beyond the pixel budget"));
        let d = doc_with_bitmap(Obj::bitmap([0, 254_000, 254_000, 0], 4, 4, 9), synth::sample_bitmap(), Limits::default());
        assert!(has_warning(&d, "bitmaps whose pixels are missing"));
        let mut unknown = synth::sample_bitmap();
        unknown[INFO..INFO + 4].copy_from_slice(&5u32.to_le_bytes());
        let d = doc_with_bitmap(obj(), unknown, Limits::default());
        assert!(has_warning(&d, "a pixel format this reader does not know"));
        // The object says 8 × 8: the 4 × 4 picture is stretched over the same frame.
        let d = doc_with_bitmap(Obj::bitmap([0, 254_000, 254_000, 0], 8, 8, 1), synth::sample_bitmap(), Limits::default());
        let Kind::Image(im) = &art(&d)[0].kind else { panic!() };
        assert!(close(im.transform.apply(Point::new(4.0, 4.0)), pt(254_000.0, 0.0)));
        assert!(has_warning(&d, "pixel size disagrees"), "{:?}", d.warnings);
        // A mask of another size is dropped: the picture is opaque.
        let mut record = synth::bmp(1, 4, 4, &Pixels::Rgb(vec![[1, 2, 3]; 16]), Some(&[0; 16]));
        let mask = 8 + u32::from_le_bytes(record[0x18..0x1C].try_into().unwrap()) as usize;
        record[mask + 14 + 8..mask + 14 + 12].copy_from_slice(&3u32.to_le_bytes());
        let d = doc_with_bitmap(obj(), record, Limits::default());
        let Kind::Image(im) = &art(&d)[0].kind else { panic!() };
        assert!(im.rgba.chunks(4).all(|p| p[3] == 255) && has_warning(&d, "masks that could not be used"));
    }
}

mod json {
    use crate::json::{Value, parse};

    #[test]
    fn values_strings_and_numbers() {
        // JSON escapes for é, an emoji as a surrogate pair, and a newline.
        let bs = "\\";
        let text = format!(r#" {{"a":{{"b":["x",1.5,true,null,{{"c":"{bs}u00e9{bs}ud83d{bs}ude00{bs}n"}}]}},"n":"42","a":"dup"}} "#);
        let v = parse(text.as_bytes()).unwrap();
        let b = v.path(&["a", "b"]).and_then(Value::as_array).unwrap();
        assert_eq!(b.len(), 5);
        assert_eq!(b[1].as_f64(), Some(1.5));
        assert_eq!(b[4].get("c").and_then(Value::as_str), Some("é😀\n"));
        // Numbers written as strings read as numbers; duplicate keys keep the first value.
        assert_eq!(v.get("n").and_then(Value::as_f64), Some(42.0));
        assert!(v.get("a").and_then(Value::as_str).is_none());
    }

    #[test]
    fn malformed_and_oversized_input_is_refused() {
        for bad in [&b"{"[..], b"{\"a\" 1}", b"[1,]", b"\"open", b"{} x", b"tru", b"1e999", b"\xff"] {
            assert!(parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let deep = "[".repeat(40) + &"]".repeat(40);
        assert!(parse(deep.as_bytes()).is_err());
        assert!(parse(&vec![b' '; crate::json::MAX_JSON + 1]).is_err());
    }
}
