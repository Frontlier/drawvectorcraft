//! CorelDRAW documents: the native reader first (pages, layers, paths, colours, gradients,
//! transparency, text, images and the user's own editable PowerClips), the converter as the
//! fallback and the embedded preview last, each step tested on its own with synthetic files.
//! Real user art stays local and is never a fixture.

use super::cdr::{CONVERTED_NOTE, NATIVE_NOTE, UNMARKED_POWERCLIPS};
use super::*;
use proptest::prelude::*;
use serde_json::{Value, json};
use std::sync::Arc;
use vectorcraft_cdr::synth::{self, Method, Obj};
use vectorcraft_color::{Color, GradientKind, Paint};
use vectorcraft_doc::appearance::AppearanceItem;
use vectorcraft_doc::{ColorMode, Document, Node, NodeId, NodeKind};
use vectorcraft_geom::{Affine, Point};
use vectorcraft_render::{RenderOptions, Renderer};

const K: f64 = vectorcraft_cdr::POINTS_PER_UNIT;
const W: f64 = synth::PAGE.0 as f64;
const H: f64 = synth::PAGE.1 as f64;

/// World units → page points of the first page (y down, origin at the page's top-left corner).
fn pt(x: f64, y: f64) -> Point {
    Point::new((x + W / 2.0) * K, (H / 2.0 - y) * K)
}

fn near(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6
}

/// A one-page document with one art layer holding `children` (first = front-most), these
/// PowerClip contents groups and these paint tables (the defaults when empty).
fn doc_bytes(children: Vec<synth::Chunk>, clip_groups: Vec<synth::Chunk>, tables: Vec<synth::Chunk>) -> Vec<u8> {
    let tables = if tables.is_empty() { synth::default_tables() } else { tables };
    let page = synth::art_page(vec![synth::layer(0, Some("Art"), 4, false, children)]);
    synth::document(2600, synth::doc_with(synth::PAGE.0, synth::PAGE.1, clip_groups, tables), vec![synth::master_page(), page]).zip(Method::Stored)
}

/// A CorelDRAW ZIP of an older layout the native reader refuses (`content/riffData.cdr`), with
/// an embedded thumbnail when `thumbnail` is given.
fn older_layout(thumbnail: Option<&[u8]>) -> Vec<u8> {
    let bare = synth::riff_inline(b"CDR9", &[synth::leaf(b"vrsn", 1300u16.to_le_bytes().to_vec())]);
    let mut entries: Vec<(&str, &[u8], Method)> =
        vec![("mimetype", synth::MIMETYPE.as_bytes(), Method::Stored), ("content/riffData.cdr", &bare, Method::Deflate)];
    if let Some(t) = thumbnail {
        entries.push(("previews/thumbnail.png", t, Method::Stored));
    }
    synth::zip(&entries)
}

/// A CorelDRAW ZIP holding only its thumbnail: the native reader and the converter's own checks
/// refuse it before any process could start, so the preview is all that opens.
fn preview_only_file() -> Vec<u8> {
    let thumb = synth::png(4, 3, [10, 200, 30]);
    synth::zip(&[("mimetype", synth::MIMETYPE.as_bytes(), Method::Stored), ("previews/thumbnail.png", &thumb, Method::Stored)])
}

fn top_art(d: &Document) -> Vec<Arc<Node>> {
    d.layers.iter().flat_map(|l| l.children().cloned().unwrap_or_default()).collect()
}

fn fill_of(n: &Node) -> Option<&Paint> {
    n.appearance.items.iter().find_map(|i| match i {
        AppearanceItem::Fill(f) => Some(&f.paint),
        AppearanceItem::Stroke(_) => None,
    })
}

fn rgb(doc: &Document, at: Point) -> [u8; 3] {
    let opts = RenderOptions { background: Some([255, 255, 255, 255]), ..Default::default() };
    let p = Renderer::new().render(doc, 900, 620, Affine::IDENTITY, &opts).pixel(at.x.round() as u32, at.y.round() as u32);
    [p[0], p[1], p[2]]
}

fn powerclips(d: &Document) -> Vec<NodeId> {
    let mut v = Vec::new();
    d.walk(|n| {
        if n.is_powerclip() {
            v.push(n.id);
        }
    });
    v
}

#[test]
fn cdr_is_a_reader_on_every_platform_with_open_and_place_filters_but_no_writer() {
    let f = format("CDR").unwrap();
    assert!(f.read && !f.write && !f.raster);
    assert!(!SAVE_FORMATS.contains(&"cdr"));
    assert!(OPEN_EXTS.contains(&"cdr") && PLACE_EXTS.contains(&"cdr"));
    assert!(open_filters().any(|(_, e)| e == ["cdr"]));
    assert!(place_filters().any(|(_, e)| e == ["cdr"]));
    let mut s = Session::new();
    s.execute("file.new", &json!({})).unwrap();
    assert!(save_plan(&s, SaveMode::SaveAs, &json!({"path": "source.cdr"})).is_err());
    assert!(s.execute("document.serialize", &json!({"format": "cdr"})).is_err());
    assert!(s.execute("document.export", &json!({"format": "cdr"})).is_err());
    assert!(save_format(None, Some("x.cdr")).is_err());
}

#[test]
fn cdr_detection_accepts_riff_and_modern_stream_names_without_treating_all_zip_as_cdr() {
    let riff = b"RIFF\x04\0\0\0CDRG";
    assert!(cdr::sniff(riff));
    assert_eq!(detect("unknown.bin", riff).unwrap().id, "cdr");
    for name in ["content/root.dat", "content/riffData.cdr"] {
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend(name.as_bytes());
        assert!(cdr::sniff(&zip));
    }
    for bytes in [b"RIFF\x04\0\0\0WEBP".as_slice(), b"PK\x03\x04word/document.xml", b"", b"CDR"] {
        assert!(!cdr::sniff(bytes));
    }
    // The native reader's sniff: a 4 KiB head (a clipboard file flavour) or the whole file, by
    // content whatever its name; ordinary ZIPs stay unclaimed; empty input is fine.
    let sample = synth::sample();
    for bytes in [&sample[..], &sample[..4096.min(sample.len())], &preview_only_file()[..]] {
        assert_eq!(detect("renamed.png", bytes).unwrap().id, "cdr");
    }
    assert!(detect("archive.zip", &synth::zip(&[("a.txt", b"hello", Method::Stored)])).is_none_or(|f| f.id != "cdr"));
    assert!(detect("epub.zip", &synth::zip(&[("mimetype", b"application/epub+zip", Method::Stored)])).is_none_or(|f| f.id != "cdr"));
    assert!(detect("x", b"").is_none());
    assert_eq!(detect("x.cdr", b"").unwrap().id, "cdr");
}

#[test]
fn invalid_cdr_does_not_replace_the_active_document() {
    let mut s = Session::new();
    s.execute("file.new", &json!({"width": 123, "height": 45})).unwrap();
    let before = vectorcraft_format::save_file(&s.doc().unwrap().doc);
    for bytes in [b"not a CDR".as_slice(), b"RIFF\x04\0\0\0CDRG", b"RIFF\xff\xff\xff\xffCDRG"] {
        assert!(open_bytes(&mut s, "damaged.cdr", bytes, None).is_err());
    }
    assert_eq!(s.documents().len(), 1);
    assert_eq!(vectorcraft_format::save_file(&s.doc().unwrap().doc), before);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn malformed_cdr_containers_are_bounded_and_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let result = std::panic::catch_unwind(|| load("damaged.cdr", &bytes));
        prop_assert!(result.is_ok());
        prop_assert!(result.unwrap().is_err());
    }
}

#[test]
fn the_native_reader_opens_by_content_whatever_the_name_and_never_saves_to_the_source() {
    let bytes = synth::sample();
    let mut s = Session::new();
    for name in ["x.cdr", "renamed.png", "unknown"] {
        let r = open_bytes(&mut s, name, &bytes, Some(format!("/source/{name}"))).unwrap();
        assert_eq!(r["format"], "cdr", "{name}");
        assert_eq!(r["restored"], false);
        assert_eq!(r["warnings"][0], NATIVE_NOTE, "{name}");
        let st = s.doc().unwrap();
        assert!(st.path.is_none(), "Save must never write the CorelDRAW source");
        assert_eq!(st.format, "vectorcraft");
        let plan = save_plan(&s, SaveMode::Save, &json!({})).unwrap();
        assert!(plan.path.is_none() && plan.name.ends_with(".vectorcraft"));
        assert_eq!(plan.format.id, "vectorcraft");
    }
    let l = load("x.cdr", &bytes).unwrap();
    assert!(!l.preview_only && !l.restored && !l.converted);
    let r = s.execute("document.open", &json!({"name": "drawing.cdr", "dataBase64": vectorcraft_format::base64_encode(&bytes)})).unwrap();
    assert_eq!(r["format"], "cdr");
    assert_eq!(r["warnings"][0], NATIVE_NOTE);
}

#[test]
fn the_sample_maps_pages_layers_paths_paints_text_images_and_guides() {
    let l = load("x.cdr", &synth::sample()).unwrap();
    let d = &l.doc;
    assert_eq!(d.artboards.len(), 1);
    assert_eq!(d.artboards[0].name, "Page 1");
    assert!((d.artboards[0].rect.width() - W * K).abs() < 1e-9 && (d.artboards[0].rect.height() - H * K).abs() < 1e-9);
    assert_eq!(d.layers.len(), 1);
    assert_eq!(d.layers[0].name.as_deref(), Some("Layer 1"));
    let kinds: Vec<&str> = top_art(d).iter().map(|n| n.kind_label()).collect();
    // Bottom to top (CorelDRAW lists the front-most first); the placeholder group is not art.
    assert_eq!(kinds, ["Image", "Type", "PowerClip", "Group", "Path", "Group", "Path", "Path", "Path"], "{kinds:?}");
    let art = top_art(d);
    // The rectangle: a linear fountain from red to CMYK black, a round-cornered 0.5 mm outline.
    let rect = &art[8];
    let Some(Paint::Gradient(g)) = fill_of(rect) else { panic!("{:?}", rect.appearance) };
    assert_eq!(g.gradient.kind, GradientKind::Linear);
    assert_eq!(g.gradient.stops.len(), 2);
    assert_eq!(g.gradient.stops[0].color, Color::Rgb { r: 1.0, g: 0.0, b: 0.0 });
    assert_eq!(g.gradient.stops[1].color, Color::Cmyk { c: 0.0, m: 0.0, y: 0.0, k: 1.0 }, "CMYK is kept as CMYK");
    let Some(AppearanceItem::Stroke(stroke)) = rect.appearance.items.last() else { panic!("{:?}", rect.appearance) };
    assert!((stroke.width - 5_000.0 * K).abs() < 1e-9, "{}", stroke.width);
    assert_eq!(stroke.join, vectorcraft_doc::appearance::LineJoin::Round);
    // The ellipse is painted with a named palette colour: a global swatch it links to.
    let Some(Paint::Solid { color, swatch: Some(name), tint }) = fill_of(&art[7]) else { panic!("{:?}", art[7].appearance) };
    assert_eq!(name, "Synth orange");
    assert_eq!(*tint, 1.0);
    let swatch = d.swatches.iter().find(|s| s.name == "Synth orange").unwrap();
    assert!(swatch.global && !swatch.spot);
    assert_eq!(swatch.paint.color(), Some(*color));
    // The wave: a radial transparency ramp as an opacity mask of grey art covering it.
    let mask = art[6].extras().mask.as_ref().expect("opacity mask");
    assert!(mask.clip);
    let Some(Paint::Gradient(ramp)) = fill_of(&mask.art) else { panic!("{:?}", mask.art.appearance) };
    assert_eq!(ramp.gradient.kind, GradientKind::Radial);
    assert!(ramp.gradient.stops.iter().all(|s| matches!(s.color, Color::Gray { .. })));
    let (b, m) = (art[6].geometric_bounds().unwrap(), mask.art.geometric_bounds().unwrap());
    assert!(m.contains(b.origin()) && m.contains(Point::new(b.x1, b.y1)), "{m:?} covers {b:?}");
    // The link group's ellipse is 50 % transparent.
    let link = art[3].children().unwrap();
    assert!((link[0].opacity - 0.5).abs() < 1e-6, "{}", link[0].opacity);
    // Artistic text: point type at its baseline, upright, in the file's font and colour.
    let NodeKind::Text(t) = &art[1].kind else { panic!() };
    assert_eq!(t.kind, vectorcraft_doc::TextKind::Point);
    assert_eq!(t.runs.len(), 1);
    assert_eq!(t.runs[0].text, "Synth € 1");
    assert!((t.runs[0].style.size - 12.0).abs() < 1e-3, "{}", t.runs[0].style.size);
    assert_eq!(t.runs[0].style.fill.color(), Some(Color::Rgb { r: 0.2, g: 0.4, b: 0.6 }));
    assert!(near(t.xf * Point::ZERO, pt(-1_000_000.0, -700_000.0)), "{:?}", t.xf);
    assert!(t.xf.determinant() > 0.0, "glyphs stay upright in the y-down document");
    // The bitmap: its 4 × 4 picture stored once, placed on its frame.
    let NodeKind::Image(im) = &art[0].kind else { panic!() };
    assert_eq!((im.width, im.height), (4, 4));
    assert_eq!(d.images.len(), 1);
    assert!(d.images.contains_key(&im.key));
    assert!(near(im.xf * Point::ZERO, pt(1_000_000.0, 754_000.0)), "{:?}", im.xf);
    // The master page's vertical guideline through the page centre.
    assert_eq!(d.guides.len(), 1);
    assert!(d.guides[0].vertical && (d.guides[0].pos - W / 2.0 * K).abs() < 1e-9);
    // Mostly RGB colours: an RGB document.
    assert_eq!(d.color_mode, ColorMode::Rgb);
}

#[test]
fn mostly_cmyk_documents_open_in_cmyk_with_their_colours_unconverted() {
    let tables = vec![synth::filt(vec![synth::fild_uniform(1, &synth::color_block(2, [10, 100, 80, 5]))]), synth::otlt(vec![synth::outl_none(1)])];
    let shapes = vec![Obj::polygon(&[(0, 0), (200_000, 0), (0, 200_000)]).chunk(), Obj::ellipse(100_000, 100_000).chunk()];
    let d = load("cmyk.cdr", &doc_bytes(shapes, vec![], tables)).unwrap().doc;
    assert_eq!(d.color_mode, ColorMode::Cmyk);
    for n in top_art(&d) {
        assert_eq!(fill_of(&n).and_then(Paint::color), Some(Color::Cmyk { c: 0.1, m: 1.0, y: 0.8, k: 0.05 }));
    }
    assert!(d.swatches.iter().filter_map(|s| s.paint.color()).any(|c| matches!(c, Color::Cmyk { .. })), "CMYK default swatches");
}

#[test]
fn gradients_are_placed_where_the_reader_puts_them() {
    let bytes = synth::sample();
    let model = vectorcraft_cdr::read(&bytes, vectorcraft_cdr::Limits::default()).unwrap();
    let rect = model.pages[0].layers[0].nodes.last().unwrap();
    let vectorcraft_cdr::Kind::Shape { fill: vectorcraft_cdr::Paint::Gradient(g), .. } = &rect.kind else { panic!("{rect:?}") };
    let m = Affine::new(g.transform.0);
    let d = load("x.cdr", &bytes).unwrap().doc;
    let art = top_art(&d);
    let Some(Paint::Gradient(gp)) = fill_of(&art[8]) else { panic!() };
    let geom = gp.geom.unwrap();
    assert!(near(geom.start, m * Point::ZERO), "{geom:?}");
    // The end lies on the isoline through the mapped (1, 0), along the normal of the isolines.
    let far = m * Point::new(1.0, 0.0);
    assert!((geom.param_at(GradientKind::Linear, far) - 1.0).abs() < 1e-9, "{geom:?}");
    assert!(gp.gradient.stops.iter().all(|s| (0.13..=0.87).contains(&s.midpoint)));
}

#[test]
fn powerclips_draw_their_frame_paint_and_clip_their_contents() {
    // The frame (blue, front-most of the file) clips a red square that sticks out of it.
    let tables = vec![
        synth::filt(vec![synth::fild_uniform(1, &synth::rgb(0, 0, 255)), synth::fild_uniform(2, &synth::rgb(255, 0, 0))]),
        synth::otlt(vec![synth::outl_none(1)]),
    ];
    let frame = Obj::polygon(&[(0, 0), (600_000, 0), (600_000, 600_000), (0, 600_000)]).powerclip(7);
    let content = Obj::polygon(&[(300_000, -300_000), (900_000, -300_000), (900_000, 300_000), (300_000, 300_000)]).fill(2);
    let bytes = doc_bytes(vec![frame.chunk()], vec![synth::clip_group(7, vec![content.chunk()])], tables);
    let d = load("clip.cdr", &bytes).unwrap().doc;
    let art = top_art(&d);
    assert_eq!(art.len(), 1);
    let pc = &art[0];
    assert!(pc.is_powerclip() && pc.clips());
    assert!(pc.extras().powerclip.unwrap().contents_locked);
    let kids = pc.children().unwrap();
    assert_eq!(kids.len(), 2, "frame first, then the contents");
    assert!(matches!(kids[0].kind, NodeKind::Path { clipping: true, .. }));
    assert_eq!(fill_of(&kids[0]).and_then(Paint::color), Some(Color::Rgb { r: 0.0, g: 0.0, b: 1.0 }), "the frame keeps its paint");
    assert_eq!(rgb(&d, pt(450_000.0, 150_000.0)), [255, 0, 0], "contents inside the frame");
    assert_eq!(rgb(&d, pt(150_000.0, 450_000.0)), [0, 0, 255], "the frame's fill behind the contents");
    assert_eq!(rgb(&d, pt(750_000.0, 0.0)), [255, 255, 255], "contents outside the frame are clipped");
}

#[test]
fn imported_powerclips_are_editable_like_ones_made_with_place_inside_frame() {
    let mut s = Session::new();
    open_bytes(&mut s, "x.cdr", &synth::sample(), None).unwrap();
    let id = powerclips(&s.doc().unwrap().doc)[0];
    let original = s.doc().unwrap().doc.node(id).unwrap().clone();
    let kids: Vec<NodeId> = original.children().unwrap().iter().map(|n| n.id).collect();
    let (frame, contents) = (kids[0], kids[1..].to_vec());
    assert_eq!(contents.len(), 2);
    let info = s.execute("object.powerClip.info", &json!({"id": id.0})).unwrap();
    assert_eq!(info["frame"], frame.0);
    assert_eq!(info["contents"], json!(contents.iter().map(|c| c.0).collect::<Vec<_>>()));
    assert_eq!(info["contentsLocked"], true);
    // Edit Contents isolates the retained contents; Finish selects the PowerClip again.
    s.execute("object.powerClip.edit", &json!({"id": id.0})).unwrap();
    assert_eq!(s.doc().unwrap().isolation, Some(id));
    assert_eq!(s.doc().unwrap().selection.objects, contents);
    s.execute("object.powerClip.finish", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().isolation, None);
    // Fit Contents moves them as one arrangement; undo puts them back.
    let before = s.doc().unwrap().doc.clone();
    s.execute("object.powerClip.fit", &json!({"id": id.0, "mode": "stretch"})).unwrap();
    let frame_box = s.doc().unwrap().doc.bounds_of(&[frame], false).unwrap();
    let fitted = s.doc().unwrap().doc.bounds_of(&contents, false).unwrap();
    assert!((fitted.width() - frame_box.width()).abs() < 1e-6 && (fitted.height() - frame_box.height()).abs() < 1e-6, "{fitted:?} {frame_box:?}");
    s.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().doc, before);
    // Extract, then Place Inside Frame with the same frame and contents: the very same PowerClip.
    s.execute("object.powerClip.extract", &json!({"id": id.0})).unwrap();
    let doc = &s.doc().unwrap().doc;
    assert!(doc.node(id).is_none() && doc.node(frame).is_some());
    assert!(matches!(doc.node(frame).unwrap().kind, NodeKind::Path { clipping: false, .. }));
    let made = s.execute("object.powerClip.make", &json!({"frame": frame.0, "ids": contents.iter().map(|c| c.0).collect::<Vec<_>>()})).unwrap();
    let remade = s.doc().unwrap().doc.node(NodeId(made["id"].as_u64().unwrap())).unwrap().clone();
    let mut expected = original.clone();
    expected.id = remade.id;
    assert_eq!(remade, expected);
}

#[test]
fn native_documents_save_and_reopen_with_their_powerclips() {
    let d = load("x.cdr", &synth::sample()).unwrap().doc;
    let saved = vectorcraft_format::save_file(&d);
    let reopened = vectorcraft_format::load_file(&saved).unwrap().doc;
    assert_eq!(vectorcraft_format::save_file(&reopened), saved);
    assert_eq!(powerclips(&reopened), powerclips(&d));
    assert_eq!(reopened.images, d.images);
    assert_eq!(reopened.swatches, d.swatches);
}

#[test]
fn objects_showing_one_picture_share_one_stored_image_and_crops_clip() {
    let a = Obj::bitmap([0, 254_000, 254_000, 0], 4, 4, 1);
    let b = Obj::bitmap([0, 254_000, 254_000, 0], 4, 4, 1).at(synth::translate(500_000.0, 0.0));
    let mut record = synth::bitmap_record([0, 254_000, 254_000, 0], 4, 4, 1);
    record[56..60].copy_from_slice(&1u32.to_le_bytes());
    let cropped = Obj::bitmap([0, 254_000, 254_000, 0], 4, 4, 1).arg(synth::ids::ARG_GEOMETRY, record).at(synth::translate(-500_000.0, 0.0));
    let page = synth::art_page(vec![synth::layer(0, Some("Art"), 4, false, vec![a.chunk(), b.chunk(), cropped.chunk()])]);
    let parts =
        synth::document_with(2600, vec![synth::sample_bitmap()], synth::doc(synth::PAGE.0, synth::PAGE.1, vec![]), vec![synth::master_page(), page]);
    let d = load("pictures.cdr", &parts.zip(Method::Stored)).unwrap().doc;
    assert_eq!(d.images.len(), 1, "one picture, stored once");
    let mut keys = Vec::new();
    d.walk(|n| {
        if let NodeKind::Image(im) = &n.kind {
            keys.push(im.key.clone());
        }
    });
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|k| *k == keys[0]));
    let art = top_art(&d);
    assert_eq!(art[0].kind_label(), "Clip Group", "the cropped bitmap");
    let clip = &art[0].children().unwrap()[0];
    assert!(matches!(clip.kind, NodeKind::Path { clipping: true, rule: vectorcraft_doc::geom::FillRule::EvenOdd, .. }));
}

/// Two pages, each with its own layer and shape; art reaching past page 1 stays clear of page 2.
fn two_pages() -> Vec<u8> {
    let tri = Obj::polygon(&[(0, 0), (200_000, 0), (0, 100_000)]).at(synth::translate(2_000_000.0, 0.0));
    let ellipse = Obj::ellipse(300_000, 300_000);
    synth::document(
        2600,
        synth::doc(synth::PAGE.0, synth::PAGE.1, vec![]),
        vec![
            synth::master_page(),
            synth::art_page(vec![synth::layer(0, Some("Front"), 4, false, vec![tri.chunk()])]),
            synth::art_page(vec![synth::layer(0, Some("Back"), 5, false, vec![ellipse.chunk()])]),
        ],
    )
    .zip(Method::Stored)
}

#[test]
fn pages_become_artboards_side_by_side_and_place_picks_one() {
    let d = load("pages.cdr", &two_pages()).unwrap().doc;
    assert_eq!(d.artboards.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["Page 1", "Page 2"]);
    let (a, b) = (d.artboards[0].rect, d.artboards[1].rect);
    assert_eq!((a.x0, a.y0), (0.0, 0.0));
    assert!((b.width() - a.width()).abs() < 1e-9 && b.y0 == 0.0);
    // Page 1's triangle reaches past its page: page 2 starts 36 pt after it.
    let tri = d.layers[0].children().unwrap()[0].children().unwrap()[0].geometric_bounds().unwrap();
    assert!(tri.x1 > a.x1);
    assert!((b.x0 - (tri.x1 + 36.0)).abs() < 1e-6, "{b:?} after {tri:?}");
    // A layer per page holds the page's layers.
    assert_eq!(d.layers.iter().map(|l| l.name.clone().unwrap()).collect::<Vec<_>>(), ["Page 1", "Page 2"]);
    assert_eq!(d.layers[1].children().unwrap()[0].name.as_deref(), Some("Back"));

    let mut s = Session::new();
    s.execute("file.new", &json!({"width": 100, "height": 100})).unwrap();
    let clean = vectorcraft_format::save_file(&s.doc().unwrap().doc);
    let data = vectorcraft_format::base64_encode(&two_pages());
    let r = s.execute("file.place", &json!({"name": "pages.cdr", "dataBase64": data, "page": 2, "link": false})).unwrap();
    assert_eq!(r["format"], "cdr");
    let placed = NodeId(r["ids"][0].as_u64().unwrap());
    let size = s.doc().unwrap().doc.bounds_of(&[placed], false).unwrap();
    let want = 300_000.0 * K;
    assert!((size.width() - want).abs() < 1e-6 && (size.height() - want).abs() < 1e-6, "page 2's ellipse: {size:?}");
    s.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(vectorcraft_format::save_file(&s.doc().unwrap().doc), clean);
    let e = s.execute("file.place", &json!({"name": "pages.cdr", "dataBase64": data, "page": 3})).unwrap_err().to_string();
    assert!(e.contains("2 page(s)"), "{e}");
    let one = vectorcraft_format::base64_encode(&synth::sample());
    let e = s.execute("file.place", &json!({"name": "one.cdr", "dataBase64": one, "page": 2})).unwrap_err().to_string();
    assert!(e.contains("1 page(s)"), "{e}");
}

#[test]
fn placing_the_native_sample_keeps_its_editable_powerclip() {
    let mut s = Session::new();
    s.execute("file.new", &json!({"width": 100, "height": 100})).unwrap();
    let r =
        s.execute("file.place", &json!({"name": "x.cdr", "dataBase64": vectorcraft_format::base64_encode(&synth::sample()), "link": false})).unwrap();
    assert_eq!(r["format"], "cdr");
    assert_eq!(powerclips(&s.doc().unwrap().doc).len(), 1);
}

const PLAIN_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100pt" height="100pt" viewBox="0 0 100 100"><rect x="10" y="10" width="50" height="50" fill="#ff0000"/></svg>"##;

const MARKED_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" data-vectorcraft-powerclip="1" width="200pt" height="200pt" viewBox="0 0 200 200">
<defs><clipPath id="pc42"><rect x="50" y="50" width="100" height="100"/></clipPath></defs>
<g id="Layer1000"><g id="VC_POWERCLIP_42">
 <g id="VC_POWERCLIP_FRAME_42"><rect x="50" y="50" width="100" height="100" fill="#0000ff"/></g>
 <g id="VC_POWERCLIP_CONTENT_42" clip-path="url(#pc42)"><rect x="40" y="50" width="60" height="100" fill="#ff0000"/></g>
</g></g></svg>"##;

#[test]
fn files_the_native_reader_refuses_go_through_the_converter() {
    let bytes = older_layout(None);
    assert!(vectorcraft_cdr::read(&bytes, vectorcraft_cdr::Limits::default()).is_err());
    let plain = |_: &[u8]| -> std::result::Result<String, String> { Ok(PLAIN_SVG.into()) };
    let i = cdr::import_with("old.cdr", &bytes, Some(&plain)).unwrap();
    assert!(!i.preview_only);
    assert_eq!(i.warnings[0], CONVERTED_NOTE);
    assert!(i.warnings[1].contains("native CDR reader could not read") && i.warnings[1].contains("riffData"), "{:?}", i.warnings);
    assert!(i.warnings.iter().any(|w| w == UNMARKED_POWERCLIPS), "stock converter output is accepted with a warning");
    let mut paths = 0;
    i.doc.walk(|n| paths += usize::from(matches!(n.kind, NodeKind::Path { .. })));
    assert!(paths > 0);
    // VectorCraft's patched converter marks PowerClips: they come back as editable PowerClips.
    let marked = |_: &[u8]| -> std::result::Result<String, String> { Ok(MARKED_SVG.into()) };
    let i = cdr::import_with("old.cdr", &bytes, Some(&marked)).unwrap();
    assert!(!i.warnings.iter().any(|w| w == UNMARKED_POWERCLIPS));
    assert_eq!(powerclips(&i.doc).len(), 1);
}

#[test]
fn when_the_converter_fails_too_the_embedded_preview_opens_alone() {
    let thumb = synth::png(5, 4, [10, 200, 30]);
    let bytes = older_layout(Some(&thumb));
    let failing = |_: &[u8]| -> std::result::Result<String, String> { Err("converter missing for this test".into()) };
    let i = cdr::import_with("old.cdr", &bytes, Some(&failing)).unwrap();
    assert!(i.preview_only);
    assert_eq!(i.warnings.len(), 1);
    let w = &i.warnings[0];
    assert!(
        w.contains("riffData") && w.contains("converter missing for this test") && w.contains("5×4") && w.contains("cannot be saved back"),
        "{w}"
    );
    assert_eq!(i.doc.images.len(), 1);
    // The converter is never asked for files it can't read either.
    let never = |_: &[u8]| -> std::result::Result<String, String> { panic!("the converter must not run") };
    assert!(cdr::import_with("x.cdr", &preview_only_file(), Some(&never)).unwrap().preview_only);
    // Turned off by the caller: the preview, saying so.
    let opts = LoadOptions { cdr_converter: false, ..LoadOptions::default() };
    let l = load_with("old.cdr", &bytes, &opts).unwrap();
    assert!(l.preview_only && l.warnings[0].contains("not used"), "{:?}", l.warnings);
}

#[test]
fn when_every_step_fails_the_error_names_the_native_reader_first() {
    let failing = |_: &[u8]| -> std::result::Result<String, String> { Err("no converter here".into()) };
    let e = cdr::import_with("old.cdr", &older_layout(None), Some(&failing)).err().unwrap().to_string();
    assert!(e.starts_with("unsupported CorelDRAW file"), "{e}");
    assert!(e.contains("no converter here") && e.contains("embedded preview"), "{e}");
}

#[test]
fn a_preview_only_file_opens_but_is_never_placed_or_used_as_a_template() {
    let bytes = preview_only_file();
    let mut s = Session::new();
    let r = open_bytes(&mut s, "x.cdr", &bytes, Some("/source/x.cdr".into())).unwrap();
    assert_eq!(r["format"], "cdr");
    assert_eq!(r["warnings"].as_array().unwrap().len(), 1);
    assert!(r["warnings"][0].as_str().unwrap().contains("embedded 4×3 PNG preview"), "{r}");
    assert!(s.doc().unwrap().path.is_none());
    s.execute("file.new", &json!({"width": 100, "height": 100})).unwrap();
    let original = s.doc().unwrap().doc.clone();
    let documents = s.documents().len();
    for name in ["x.cdr", "renamed.png", "renamed.txt"] {
        let p = json!({"name": name, "dataBase64": vectorcraft_format::base64_encode(&bytes), "link": true});
        for cmd in ["file.place", "file.place.info"] {
            let e = s.execute(cmd, &p).unwrap_err().to_string();
            assert!(e.contains("File › Open") && e.contains("embedded preview"), "{cmd} {name}: {e}");
        }
        let e = s.execute("file.place.queue", &json!({"files": [p]})).unwrap_err().to_string();
        assert!(e.contains("File › Open") && e.contains("embedded preview"), "{e}");
        for cmd in ["file.newFromTemplate", "swatch.library.load", "graphicStyle.loadLibrary"] {
            let e = s.execute(cmd, &p).unwrap_err().to_string();
            assert!(e.contains("File › Open") && e.contains("embedded preview"), "{cmd}: {e}");
        }
    }
    assert_eq!(s.documents().len(), documents);
    assert_eq!(s.doc().unwrap().doc, original);
}

/// Run explicitly with VECTORCRAFT_CDR_TEST_FILE set to a user-owned local CDR (a file, or a
/// folder searched for `.cdr` files). Opens natively through the shared engine; edits, undo,
/// PowerClips, native round-trip, Place and source preservation. Prints counts only.
#[test]
#[ignore = "requires a local user-owned CDR (VECTORCRAFT_CDR_TEST_FILE)"]
fn local_cdr_opens_as_editable_art_and_saves_without_changing_its_source() {
    let Some(root) = std::env::var_os("VECTORCRAFT_CDR_TEST_FILE") else {
        eprintln!("VECTORCRAFT_CDR_TEST_FILE is not set: skipped");
        return;
    };
    let root = std::path::PathBuf::from(root);
    let mut files = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            stack.extend(std::fs::read_dir(&p).unwrap().flatten().map(|e| e.path()));
        } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cdr")) {
            files.push(p);
        }
    }
    assert!(!files.is_empty(), "no .cdr file in VECTORCRAFT_CDR_TEST_FILE");
    for (n, file) in files.iter().enumerate() {
        local_cdr(n, &file.to_string_lossy());
    }
}

fn local_cdr(n: usize, path: &str) {
    let bytes = std::fs::read(path).unwrap();
    let mut s = Session::new();
    let started = std::time::Instant::now();
    let r = s.execute("document.open", &json!({"path": path})).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(r["format"], "cdr");
    let warnings: Vec<&str> = r["warnings"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(warnings[0], NATIVE_NOTE, "read natively, not through the converter or the preview: {warnings:?}");
    let st = s.doc().unwrap();
    assert!(st.path.is_none());
    assert_eq!(st.format, "vectorcraft");
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    let mut masks = 0;
    st.doc.walk(|n| {
        *counts.entry(n.kind_label()).or_default() += 1;
        masks += usize::from(n.extras().mask.is_some());
    });
    eprintln!(
        "file {n}: opened in {elapsed:.2?}; {} artboard(s), {} layer(s), colour mode {:?}, {} image(s) stored, {} swatches, {} guides, {masks} opacity masks",
        st.doc.artboards.len(),
        st.doc.layers.len(),
        st.doc.color_mode,
        st.doc.images.len(),
        st.doc.swatches.len(),
        st.doc.guides.len()
    );
    eprintln!("file {n}: objects by kind: {counts:?}");
    for w in &warnings {
        eprintln!("file {n}: warning: {w}");
    }
    let paths = counts
        .iter()
        .filter(|(k, _)| !matches!(**k, "Layer" | "Group" | "Clip Group" | "PowerClip" | "Type" | "Image"))
        .map(|(_, v)| v)
        .sum::<usize>();
    assert!(paths > 0, "editable paths, not a preview bitmap");
    // PowerClips are the user's own: their commands work on them.
    if let Some(id) = powerclips(&st.doc).first().copied() {
        let info = s.execute("object.powerClip.info", &json!({"id": id.0})).unwrap();
        assert_eq!(info["id"], id.0);
        s.execute("object.powerClip.edit", &json!({"id": id.0})).unwrap();
        s.execute("object.powerClip.finish", &json!({})).unwrap();
    }
    let before = vectorcraft_format::save_file(&s.doc().unwrap().doc);
    s.execute("shape.rectangle", &json!({"x": 0, "y": 0, "width": 10, "height": 10})).unwrap();
    assert_ne!(vectorcraft_format::save_file(&s.doc().unwrap().doc), before);
    s.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(vectorcraft_format::save_file(&s.doc().unwrap().doc), before);
    let native = s.execute("document.serialize", &json!({"format": "vectorcraft"})).unwrap();
    let native = vectorcraft_format::base64_decode(native["dataBase64"].as_str().unwrap()).unwrap();
    let reopened = vectorcraft_format::load_file(&native).unwrap();
    assert_eq!(vectorcraft_format::save_file(&reopened.doc), before);
    let save = save_plan(&s, SaveMode::Save, &json!({})).unwrap();
    assert!(save.path.is_none() && save.name.ends_with(".vectorcraft"));
    assert!(s.execute("document.save", &json!({"path": path})).is_err());
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    let mut placed = Session::new();
    placed.execute("file.new", &json!({"width": 100, "height": 100})).unwrap();
    let clean = vectorcraft_format::save_file(&placed.doc().unwrap().doc);
    let r = placed.execute("file.place", &json!({"path": path, "link": false})).unwrap();
    assert_eq!(r["format"], "cdr");
    assert!(!r["ids"].as_array().unwrap().is_empty());
    let mut paths = 0;
    placed.doc().unwrap().doc.walk(|n| paths += usize::from(matches!(n.kind, NodeKind::Path { .. })));
    assert!(paths > 0, "placing CDR must retain the pasteboard vectors");
    placed.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(vectorcraft_format::save_file(&placed.doc().unwrap().doc), clean);
}
