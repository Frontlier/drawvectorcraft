//! CorelDRAW documents the user owns, read locally and never committed. `VECTORCRAFT_CDR_TEST_FILE`
//! names one `.cdr` file or a folder searched recursively for `*.cdr`; without it the tests pass
//! without doing anything. Only counts are printed, never names, text or other content.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use vectorcraft_cdr::zip::Zip;
use vectorcraft_cdr::{Blend, Color, Document, Kind, Limits, Node, POINTS_PER_UNIT, Paint, Rect, is_cdr, preview, read, version};

const VAR: &str = "VECTORCRAFT_CDR_TEST_FILE";

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("cdr")) {
            out.push(p);
        }
    }
}

fn samples() -> Vec<PathBuf> {
    let Some(path) = std::env::var_os(VAR).map(PathBuf::from) else {
        eprintln!("{VAR} is not set: no CorelDRAW files to check");
        return Vec::new();
    };
    let mut out = Vec::new();
    if path.is_dir() {
        collect(&path, &mut out);
    } else if path.is_file() {
        out.push(path.clone());
    }
    if out.is_empty() {
        eprintln!("{VAR} names no .cdr file: {}", path.display());
    }
    out.sort();
    out
}

/// The split layout (`content/root.dat`) is the one the reader supports; older layouts are refused
/// by design so the caller can use another importer.
fn is_split_layout(bytes: &[u8]) -> bool {
    Zip::open(bytes, Limits::default()).is_ok_and(|z| z.entry("content/root.dat").is_some())
}

#[derive(Debug, Default)]
struct Stats {
    groups: usize,
    shapes: usize,
    texts: usize,
    images: usize,
    powerclips: usize,
    /// Leaf objects: shapes, texts and images (PowerClip frames included).
    objects: usize,
    fills_solid: usize,
    fills_gradient: usize,
    fills_none: usize,
    fills_unsupported: usize,
    /// Bitmap objects whose picture could not be decoded (their outline stands in).
    bitmap_placeholders: usize,
    named_colors: usize,
    outlines: usize,
    translucent: usize,
    masks: usize,
    blended: usize,
    texts_without_runs: usize,
    runs: usize,
    characters: usize,
    images_cropped: usize,
    image_pixels: u64,
    image_sizes: BTreeSet<(u32, u32)>,
    bbox_compared: usize,
    bbox_within_2_5: usize,
    bbox_within_10: usize,
    bbox_worse: usize,
    bbox_worst_units: f64,
}

/// Largest distance between two rectangles' edges, in CorelDRAW world units.
fn rect_error(a: Rect, b: Rect) -> f64 {
    [a.x0 - b.x0, a.y0 - b.y0, a.x1 - b.x1, a.y1 - b.y1].iter().map(|d| d.abs()).fold(0.0, f64::max) / POINTS_PER_UNIT
}

fn count_paint(p: &Paint, s: &mut Stats) {
    match p {
        Paint::None => s.fills_none += 1,
        Paint::Solid(c) => {
            s.fills_solid += 1;
            if matches!(c, Color::Named { .. }) {
                s.named_colors += 1;
            }
        }
        Paint::Gradient(_) => s.fills_gradient += 1,
        Paint::Unsupported("bitmap") => s.bitmap_placeholders += 1,
        Paint::Unsupported(_) => s.fills_unsupported += 1,
    }
}

fn walk(n: &Node, s: &mut Stats) {
    if n.opacity < 1.0 {
        s.translucent += 1;
    }
    if n.opacity_mask.is_some() {
        s.masks += 1;
    }
    if n.blend != Blend::Normal {
        s.blended += 1;
    }
    match &n.kind {
        Kind::Group { children } => {
            s.groups += 1;
            children.iter().for_each(|c| walk(c, s));
        }
        Kind::Shape { path, fill, outline, .. } => {
            s.shapes += 1;
            s.objects += 1;
            count_paint(fill, s);
            s.outlines += usize::from(outline.is_some());
            if matches!(fill, Paint::Unsupported("bitmap")) {
                return;
            }
            if let (Some(stored), Some(drawn)) = (n.bounds, path.bounds()) {
                let e = rect_error(stored, drawn);
                s.bbox_compared += 1;
                s.bbox_worst_units = s.bbox_worst_units.max(e);
                if e <= 2.5 {
                    s.bbox_within_2_5 += 1;
                } else if e <= 10.0 {
                    s.bbox_within_10 += 1;
                } else {
                    s.bbox_worse += 1;
                }
            }
        }
        Kind::Text(t) => {
            s.texts += 1;
            s.objects += 1;
            s.texts_without_runs += usize::from(t.runs.is_empty());
            s.runs += t.runs.len();
            s.characters += t.runs.iter().map(|r| r.text.chars().count()).sum::<usize>();
            for r in &t.runs {
                count_paint(&r.fill, s);
                s.outlines += usize::from(r.outline.is_some());
            }
        }
        Kind::Image(im) => {
            s.images += 1;
            s.objects += 1;
            s.images_cropped += usize::from(im.clip.is_some());
            s.image_pixels += u64::from(im.width) * u64::from(im.height);
            s.image_sizes.insert((im.width, im.height));
            assert_eq!(im.rgba.len() as u64, u64::from(im.width) * u64::from(im.height) * 4, "image buffer size");
        }
        Kind::PowerClip { frame, contents, .. } => {
            s.powerclips += 1;
            walk(frame, s);
            contents.iter().for_each(|c| walk(c, s));
        }
    }
}

fn summary(doc: &Document) -> (Stats, String) {
    let mut s = Stats::default();
    let mut lines = vec![format!("version {}, {} page(s)", doc.version, doc.pages.len())];
    for (i, p) in doc.pages.iter().enumerate() {
        let before = s.objects;
        for l in &p.layers {
            l.nodes.iter().for_each(|n| walk(n, &mut s));
        }
        lines.push(format!(
            "  page {}: {:.1} x {:.1} pt, {} layer(s), {} guide(s), {} object(s)",
            i + 1,
            p.width,
            p.height,
            p.layers.len(),
            p.guides.len(),
            s.objects - before
        ));
    }
    lines.push(format!(
        "  objects {} (shapes {}, texts {}, images {}), groups {}, PowerClips {}",
        s.objects, s.shapes, s.texts, s.images, s.groups, s.powerclips
    ));
    lines.push(format!(
        "  paint: {} solid ({} named swatches), {} gradient, {} none, {} unsupported; {} visible outlines",
        s.fills_solid, s.named_colors, s.fills_gradient, s.fills_none, s.fills_unsupported, s.outlines
    ));
    lines.push(format!("  transparency: {} translucent, {} opacity masks, {} with a blend mode", s.translucent, s.masks, s.blended));
    lines.push(format!("  text: {} run(s), {} character(s), {} text(s) without characters", s.runs, s.characters, s.texts_without_runs));
    lines.push(format!(
        "  bitmaps: {} decoded ({} cropped, {} distinct picture sizes, {} pixels), {} shown as their outline",
        s.images,
        s.images_cropped,
        s.image_sizes.len(),
        s.image_pixels,
        s.bitmap_placeholders
    ));
    lines.push(format!(
        "  stored bbox vs path extents: {} compared, {} within 2.5 units, {} within 10, {} worse (worst {:.1} units)",
        s.bbox_compared, s.bbox_within_2_5, s.bbox_within_10, s.bbox_worse, s.bbox_worst_units
    ));
    lines.push(format!("  {} warning line(s):", doc.warnings.len()));
    lines.extend(doc.warnings.iter().map(|w| format!("    {w}")));
    (s, lines.join("\n"))
}

fn warned(doc: &Document, about: &str) -> bool {
    doc.warnings.iter().any(|w| w.contains(about))
}

#[test]
fn user_files_read_completely() {
    let paths = samples();
    let mut failures = Vec::new();
    let mut read_count = 0;
    for (i, p) in paths.iter().enumerate() {
        let bytes = std::fs::read(p).unwrap();
        // Files are numbered, not named, so nothing about them reaches the log.
        let label = format!("file {}", i + 1);
        assert!(is_cdr(&bytes), "{label}: not recognised");
        assert!(is_cdr(&bytes[..bytes.len().min(4096)]), "{label}: not recognised from a 4 KiB head");
        if !is_split_layout(&bytes) {
            let refused = read(&bytes, Limits::default());
            eprintln!("{label}: older layout, refused as designed: {:?}", refused.err());
            continue;
        }
        match read(&bytes, Limits::default()) {
            Ok(doc) => {
                read_count += 1;
                let (s, text) = summary(&doc);
                eprintln!("{label}: {text}");
                assert!(!doc.pages.is_empty(), "{label}: no pages");
                assert!(s.objects > 0, "{label}: no objects");
                assert_eq!(version(&bytes).ok(), Some(doc.version), "{label}: version");
                // CorelDRAW's stored bounds are the tight Bézier extents of the placed geometry:
                // allow the odd generated object (artistic media) to differ.
                let off = s.bbox_worse;
                assert!(off * 200 <= s.bbox_compared.max(200), "{label}: {off} of {} objects disagree with their stored bounds", s.bbox_compared);
                // Nothing is dropped silently: whatever was not decoded is in the warnings.
                assert!(s.texts_without_runs == 0 || warned(&doc, "text"), "{label}: texts without characters and no warning");
                assert!(s.bitmap_placeholders == 0 || warned(&doc, "bitmap"), "{label}: undecoded bitmaps and no warning");
                assert!(s.fills_unsupported == 0 || warned(&doc, "not imported"), "{label}: unsupported paint and no warning");
                match preview(&bytes) {
                    Ok(p) => eprintln!("{label}: preview {} x {} ({} bytes)", p.width, p.height, p.png.len()),
                    Err(e) => eprintln!("{label}: no usable preview: {e}"),
                }
            }
            Err(e) => failures.push(format!("{label}: {e}")),
        }
    }
    assert!(failures.is_empty(), "{} of {} failed:\n{}", failures.len(), paths.len(), failures.join("\n"));
    if !paths.is_empty() {
        eprintln!("{read_count} of {} CorelDRAW file(s) read natively", paths.len());
    }
}

#[test]
fn user_files_survive_tight_limits() {
    // A budget far below the file's size must give a clean error or a document, never a panic;
    // a tiny pixel budget leaves bitmaps to their outlines with a warning.
    for p in samples() {
        let bytes = std::fs::read(p).unwrap();
        for limits in [
            Limits { max_entry: 1 << 20, ..Limits::default() },
            Limits { max_total: 4 << 20, ..Limits::default() },
            Limits { max_pixels: 1, ..Limits::default() },
        ] {
            if let Ok(doc) = read(&bytes, limits)
                && limits.max_pixels == 1
            {
                let (s, _) = summary(&doc);
                assert!(s.images == 0 && (s.bitmap_placeholders == 0 || warned(&doc, "pixel budget")));
            }
        }
    }
}
