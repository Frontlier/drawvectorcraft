//! Artistic text.
//!
//! A text object (loda objType 4) keeps its characters and style in a `txsm` story that is a
//! child of the same `obj ` list. Its fonts are listed in `doc/fntt` and `font/fontTable.dat`
//! (names only: the font programs are never in the file) and its styles in `doc/stlt`.
//!
//! ```text
//! txsm: u32 head[9] | u32 formatter @0x24 | u8 @0x28 | u32 @0x29 | u32 textId @0x2D
//!       f64 matrix[6] @0x31 (= the object's trfd) | 12 bytes
//!       u32 nObjectStyles @0x6D, each {u32 styleId, u16 flags, u32 len, UTF-8 JSON}
//!       u32 nCharStyles, each {u32 useCount, u16 flags, u32 n + n UTF-16 units (language), u32 len, JSON}
//!       u32 nChars, nChars × 8 bytes (all zero in the sample) | u32 textLen, textLen bytes (Windows-1252)
//! ```
//!
//! The style JSON holds strings with numbers: `character.<latin|farEast|RTL>.{font, size (em size
//! in world units), weight (bit field), italic, underline}`, `character.fill/outline` and
//! `paragraph.interCharSpacing` (×1e6 of the space width). The character style overrides the
//! object style key by key. The anchor is the start of the first baseline (local (0, 0)).

use std::cell::OnceCell;
use std::collections::HashMap;

use crate::container::{Container, attribute, unescape};
use crate::geometry::{Decode, arg, f64_at, u16_at, u32_at, utf16z};
use crate::json::{self, Value};
use crate::model::{Affine, Align, Color, POINTS_PER_UNIT, Paint, Run, Text, Warnings};
use crate::paint::{self, Target};
use crate::style::FontRecord;

const MAX_STYLES: usize = 4096;
const MAX_CHARS: usize = 1 << 20;
const MAX_LANGUAGE: usize = 64;
const MAX_FONTS: usize = 65_536;
const MAX_FONT_NAME: usize = 256;
const MAX_FONT_TABLE: usize = 4 << 20;
const MAX_TEXT_INDEX: usize = 16 << 20;
/// CorelDRAW's default size, 12 pt, in world units.
const DEFAULT_SIZE: f64 = 12.0 / POINTS_PER_UNIT;
/// The space advance assumed when turning character spacing into em (INFERRED: the spacing of
/// the sample's labels grows by about 0.3 em per 100 %, the width of a space).
const SPACE_EM: f64 = 0.3;
/// The story header of the sample, except word 5 (14 there: a count of texts or styles).
const HEAD: [u32; 9] = [1, 1, 0, 1, 8, 0, 0, 3, 0];
/// Script slots of a text style: latin, far-east (CJK) and right-to-left.
const SLOTS: [&str; 3] = ["latin", "farEast", "RTL"];

/// Windows-1252 bytes 0x80..0x9F (U+FFFD where the code page has no character).
const CP1252_HIGH: [u16; 32] = [
    0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0xFFFD, 0x017D, 0xFFFD, 0xFFFD, 0x2018,
    0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0xFFFD, 0x017E, 0x0178,
];

#[derive(Debug, Clone, PartialEq)]
struct Font {
    family: String,
    postscript: Option<String>,
}

/// Fonts and text stories of the document.
#[derive(Debug, Default)]
pub(crate) struct Tables {
    fonts: HashMap<u32, Font>,
    /// Font ids in list order (the first is the fallback family).
    order: Vec<u32>,
    /// Each `txsm` chunk's position among all stories, in file order.
    stories: HashMap<usize, usize>,
    /// `META-INF/textinfo.xml`, read only when a story's characters are in an unverified layout.
    index: OnceCell<Option<Vec<String>>>,
}

impl Tables {
    /// Index the fonts (`doc` is the `doc ` list) and the stories.
    pub(crate) fn load(c: &Container<'_>, doc: usize, w: &mut Warnings) -> Self {
        let mut t = Self::default();
        if let Some(fntt) = c.tree.child_list(doc, b"fntt") {
            for leaf in c.tree.children(fntt) {
                if !c.tree.get(leaf).is_some_and(|ch| !ch.list && &ch.id == b"font") {
                    continue;
                }
                if t.fonts.len() >= MAX_FONTS {
                    break;
                }
                match c.payload(leaf).ok().and_then(font_record) {
                    Some((id, f)) => t.add(id, f),
                    None => w.warn("font records that could not be read (ignored)"),
                }
            }
        }
        if let Some(table) = c.side_entry("font/fontTable.dat", MAX_FONT_TABLE) {
            match font_table(&table) {
                Some(list) => {
                    for (id, f) in list {
                        if let Some(known) = t.fonts.get_mut(&id) {
                            if known.postscript.is_none() {
                                known.postscript = f.postscript;
                            }
                        } else if t.fonts.len() < MAX_FONTS {
                            t.add(id, f);
                        }
                    }
                }
                None => w.warn("the font table could not be read (fonts are named by family only)"),
            }
        }
        let stories = c.tree.chunks().iter().enumerate().filter(|(_, ch)| !ch.list && &ch.id == b"txsm").map(|(i, _)| i);
        t.stories = stories.enumerate().map(|(n, i)| (i, n)).collect();
        t
    }

    fn add(&mut self, id: u32, f: Font) {
        if !self.fonts.contains_key(&id) {
            self.order.push(id);
            self.fonts.insert(id, f);
        }
    }

    /// The PostScript name of a family, from the font lists.
    fn postscript(&self, family: &str, preferred: Option<u32>) -> Option<String> {
        if let Some(f) = preferred.and_then(|id| self.fonts.get(&id)).filter(|f| f.family == family) {
            return f.postscript.clone();
        }
        self.order.iter().filter_map(|id| self.fonts.get(id)).find(|f| f.family == family).and_then(|f| f.postscript.clone())
    }

    /// The characters of story chunk `leaf` from the document's text index, when it has one
    /// entry per story.
    fn indexed(&self, c: &Container<'_>, leaf: usize) -> Option<String> {
        let n = *self.stories.get(&leaf)?;
        let index = self.index.get_or_init(|| {
            let xml = c.side_entry("META-INF/textinfo.xml", MAX_TEXT_INDEX)?;
            let streams = text_streams(&String::from_utf8_lossy(&xml));
            (streams.len() == self.stories.len()).then_some(streams)
        });
        index.as_ref()?.get(n).cloned()
    }
}

/// `doc/fntt/font` (296 bytes): u32 fontId, u32 weight, PANOSE, family as 32 UTF-16 units @0x12,
/// …, PostScript name as 64 units @0xA4.
fn font_record(b: &[u8]) -> Option<(u32, Font)> {
    if b.len() < 0x128 {
        return None;
    }
    let id = u32_at(b, 0)?;
    let family = utf16z(b.get(0x12..0x52)?)?;
    Some((id, Font { family, postscript: utf16z(b.get(0xA4..0x124)?) }))
}

/// `font/fontTable.dat`: u32 version, u32 rest length, u32 count, then records {u32 kind, u32 len,
/// u32 fontId, u32 weight, u8 PANOSE[10], counted family, u8 charset, counted script, u32, u32
/// technology, counted PostScript name, counted version, counted style}. A damaged record is
/// skipped.
fn font_table(b: &[u8]) -> Option<Vec<(u32, Font)>> {
    let rest = u32_at(b, 4)? as usize;
    if rest.checked_add(8) != Some(b.len()) {
        return None;
    }
    let count = (u32_at(b, 8)? as usize).min(MAX_FONTS);
    let mut at = 12usize;
    let mut out = Vec::new();
    for _ in 0..count {
        let len = u32_at(b, at + 4)? as usize;
        let r = b.get(at + 8..(at + 8).checked_add(len)?)?;
        at += 8 + len;
        let record = || -> Option<(u32, Font)> {
            let id = u32_at(r, 0)?;
            let (family, next) = counted(r, 18)?;
            let (_, next) = counted(r, next + 1)?;
            let (postscript, _) = counted(r, next + 8)?;
            let family = family.filter(|f| !f.trim().is_empty())?;
            Some((id, Font { family, postscript }))
        };
        out.extend(record());
    }
    Some(out)
}

/// `u32 n` then `n` UTF-16 units (the count includes a terminating NUL, which is dropped).
fn counted(b: &[u8], at: usize) -> Option<(Option<String>, usize)> {
    let n = u32_at(b, at)? as usize;
    if n > MAX_FONT_NAME {
        return None;
    }
    let end = at + 4 + 2 * n;
    Some((utf16z(b.get(at + 4..end)?), end))
}

/// A bounded forward reader over a story.
struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

const SHORT: &str = "a truncated text story";

impl<'a> Cursor<'a> {
    fn u16(&mut self) -> Decode<u16> {
        let v = u16_at(self.b, self.at).ok_or(SHORT)?;
        self.at += 2;
        Ok(v)
    }

    fn u32(&mut self) -> Decode<u32> {
        let v = u32_at(self.b, self.at).ok_or(SHORT)?;
        self.at += 4;
        Ok(v)
    }

    fn count(&mut self, cap: usize, what: &'static str) -> Decode<usize> {
        let n = self.u32()? as usize;
        if n > cap {
            return Err(what);
        }
        Ok(n)
    }

    fn bytes(&mut self, n: usize) -> Decode<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(SHORT)?;
        let s = self.b.get(self.at..end).ok_or(SHORT)?;
        self.at = end;
        Ok(s)
    }

    /// `u32 len` + JSON; `None` when the JSON does not parse.
    fn json(&mut self) -> Decode<Option<Value>> {
        let n = self.count(json::MAX_JSON, "a text style that is too long")?;
        Ok(json::parse(self.bytes(n)?).ok())
    }
}

#[derive(Debug)]
struct Story<'a> {
    head_ok: bool,
    matrix: Option<[f64; 6]>,
    object_styles: Vec<(u32, Option<Value>)>,
    char_styles: Vec<Option<Value>>,
    chars: usize,
    records_zero: bool,
    text: &'a [u8],
}

fn story(b: &[u8]) -> Decode<Story<'_>> {
    if b.len() < 0x71 {
        return Err(SHORT);
    }
    let head_ok = HEAD.iter().enumerate().all(|(k, v)| k == 5 || u32_at(b, 4 * k) == Some(*v));
    let mut m = [0.0; 6];
    for (k, v) in m.iter_mut().enumerate() {
        *v = f64_at(b, 0x31 + 8 * k).ok_or(SHORT)?;
    }
    let matrix = m.iter().all(|v| v.is_finite()).then_some(m);
    let mut c = Cursor { b, at: 0x6D };
    let n = c.count(MAX_STYLES, "a text with too many styles")?;
    let mut object_styles = Vec::with_capacity(n.min(16));
    for _ in 0..n {
        let id = c.u32()?;
        c.u16()?;
        object_styles.push((id, c.json()?));
    }
    let n = c.count(MAX_STYLES, "a text with too many styles")?;
    let mut char_styles = Vec::with_capacity(n.min(16));
    for _ in 0..n {
        c.u32()?;
        c.u16()?;
        let units = c.count(MAX_LANGUAGE, "a text language tag that is too long")?;
        c.bytes(2 * units)?;
        char_styles.push(c.json()?);
    }
    let chars = c.count(MAX_CHARS, "a text with too many characters")?;
    let records = c.bytes(chars.checked_mul(8).ok_or(SHORT)?)?;
    let len = c.count(4 * MAX_CHARS, "a text with too many characters")?;
    let text = c.bytes(len)?;
    Ok(Story { head_ok, matrix, object_styles, char_styles, chars, records_zero: records.iter().all(|v| *v == 0), text })
}

/// Windows-1252 (the ANSI code page of the sample's fonts), NULs dropped.
fn cp1252(b: &[u8]) -> String {
    b.iter()
        .filter(|c| **c != 0)
        .map(|&c| match c {
            0x80..=0x9F => CP1252_HIGH.get(usize::from(c - 0x80)).and_then(|u| char::from_u32(u32::from(*u))).unwrap_or(char::REPLACEMENT_CHARACTER),
            _ => char::from(c),
        })
        .collect()
}

fn utf16le(b: &[u8]) -> String {
    let units = b.as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p)).filter(|u| *u != 0);
    char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
}

/// `META-INF/textinfo.xml`: one `<TextStream>` per story in file order, each holding
/// `<TextRun …>text</TextRun>` elements; a run with `break="para"` ends a paragraph.
fn text_streams(xml: &str) -> Vec<String> {
    fn element<'x>(s: &'x str, name: &str) -> Option<(usize, &'x str)> {
        let mut from = 0;
        while let Some(at) = s.get(from..)?.find(name) {
            let start = from + at;
            let after = s.get(start + name.len()..)?;
            if after.starts_with(|c: char| c == '>' || c == '/' || c.is_whitespace()) {
                return Some((start, after));
            }
            from = start + name.len();
        }
        None
    }
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some((_, after)) = element(rest, "<TextStream") {
        let end = after.find("</TextStream>").unwrap_or(after.len());
        let mut body = after.get(..end).unwrap_or_default();
        rest = after.get(end..).unwrap_or_default().strip_prefix("</TextStream>").unwrap_or_default();
        let mut text = String::new();
        let mut paragraph_ended = false;
        while let Some((_, run)) = element(body, "<TextRun") {
            let Some(gt) = run.find('>') else { break };
            let attrs = run.get(..gt).unwrap_or_default();
            let content = run.get(gt + 1..).unwrap_or_default();
            if attrs.ends_with('/') {
                body = content;
                continue;
            }
            let close = content.find("</TextRun>").unwrap_or(content.len());
            if paragraph_ended {
                text.push('\n');
            }
            text.push_str(&unescape(content.get(..close).unwrap_or_default()));
            paragraph_ended = attribute(attrs, "break") == Some("para");
            body = content.get(close..).unwrap_or_default();
        }
        out.push(text);
        if out.len() >= MAX_CHARS {
            break;
        }
    }
    out
}

/// CSS weight of a weight bit field: bit `b` set alone means weight (b/2 + 1)·100 (400, 500 and
/// 900 were verified against family names; the rest is INFERRED).
pub(crate) fn css_weight(bits: u32, w: &mut Warnings) -> u16 {
    if bits.is_power_of_two() {
        let class = (bits.trailing_zeros() / 2 + 1) * 100;
        return u16::try_from(class.clamp(100, 900)).unwrap_or(400);
    }
    w.warn("font weights this reader does not know (regular used)");
    400
}

/// Which style slot draws a character: far-east (CJK) or right-to-left scripts, else latin.
/// Characters without a script (spaces, punctuation, digits) stay with the slot before them.
fn slot_of(c: char, previous: usize) -> usize {
    match u32::from(c) {
        0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF => 2,
        0x1100..=0x11FF
        | 0x2E80..=0x9FFF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7FF
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFFEF
        | 0x20000..=0x3FFFF => 1,
        _ if !c.is_alphabetic() => previous,
        _ => 0,
    }
}

/// One script slot of a text style, resolved.
#[derive(Debug, Clone)]
struct Face {
    family: String,
    postscript: Option<String>,
    /// World units.
    size: f64,
    weight: Option<u32>,
    italic: bool,
    underline: bool,
}

struct Style<'s> {
    object: Option<&'s Value>,
    character: Option<&'s Value>,
    font: Option<FontRecord>,
    tables: &'s Tables,
}

impl Style<'_> {
    /// A key of the character style, else of the object style.
    fn get(&self, path: &[&str]) -> Option<&Value> {
        self.character.and_then(|v| v.path(path)).or_else(|| self.object.and_then(|v| v.path(path)))
    }

    fn face(&self, slot: usize, latin: Option<&Face>) -> Face {
        let name = SLOTS.get(slot).copied().unwrap_or("latin");
        let attr = |key: &str| self.get(&["character", name, key]);
        let font_id = self.font.and_then(|f| f.font_ids.get(slot).copied());
        let stored = |list: Option<[u32; 3]>| list.and_then(|l| l.get(slot).copied()).filter(|v| *v != 0);
        let family = attr("font")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| font_id.and_then(|id| self.tables.fonts.get(&id)).map(|f| f.family.clone()))
            .or_else(|| latin.map(|l| l.family.clone()))
            .or_else(|| self.tables.order.first().and_then(|id| self.tables.fonts.get(id)).map(|f| f.family.clone()))
            .unwrap_or_default();
        let size = attr("size")
            .and_then(Value::as_f64)
            .filter(|v| *v > 0.0)
            .or_else(|| stored(self.font.map(|f| f.sizes)).map(f64::from))
            .or_else(|| latin.map(|l| l.size))
            .unwrap_or(DEFAULT_SIZE);
        let weight = attr("weight")
            .and_then(Value::as_f64)
            .filter(|v| (0.0..=f64::from(u32::MAX)).contains(v))
            .map(|v| v as u32)
            .or_else(|| stored(self.font.map(|f| f.weights)))
            .or_else(|| latin.and_then(|l| l.weight));
        let on = |key: &str| attr(key).and_then(Value::as_f64).map(|v| v != 0.0);
        Face {
            postscript: self.tables.postscript(&family, font_id),
            family,
            size,
            weight,
            italic: on("italic").or_else(|| latin.map(|l| l.italic)).unwrap_or(false),
            underline: on("underline").or_else(|| latin.map(|l| l.underline)).unwrap_or(false),
        }
    }
}

fn same_placement(a: &Affine, b: &Affine) -> bool {
    a.0.iter().zip(b.0.iter()).all(|(x, y)| (x - y).abs() <= 1e-6 * (1.0 + x.abs().max(y.abs())))
}

/// The text of the object list `obj` (its `txsm` child). `transform` maps text space (origin at
/// the first baseline's start, y down, points) to page space; a uniform scale in it is moved into
/// the run sizes.
pub(crate) fn decode(
    tables: &Tables,
    paint: &paint::Tables,
    c: &Container<'_>,
    obj: usize,
    t: &Target<'_, '_>,
    transform: Affine,
    w: &mut Warnings,
) -> Text {
    let s = transform.scale();
    let (scale, transform) =
        if s.is_finite() && s > 1e-9 { (s, Affine::scale_non_uniform(1.0 / s, 1.0 / s).then(transform)) } else { (1.0, transform) };
    let mut out = Text { runs: Vec::new(), align: Align::Left, transform };
    let Some(leaf) = c.tree.child_leaf(obj, b"txsm") else {
        w.warn("text objects without their characters (not imported)");
        return out;
    };
    let st = match c.payload(leaf).map_err(|_| "a story that could not be read").and_then(story) {
        Ok(st) => st,
        Err(e) => {
            w.warn(format!("text that could not be decoded: {e} (not imported)"));
            return out;
        }
    };
    if !st.head_ok {
        w.warn("text stories with an unfamiliar header (read anyway)");
    }
    if st.matrix.is_some_and(|m| !same_placement(&Affine::from_cdr(m), &t.world)) {
        w.warn("text stories placed differently from their object (the object's placement is used)");
    }
    let (style_id, object) = st.object_styles.first().map_or((None, None), |(id, json)| (Some(*id), json.as_ref()));
    let character = st.char_styles.first().and_then(Option::as_ref);
    if st.object_styles.first().is_some_and(|s| s.1.is_none()) || st.char_styles.first().is_some_and(Option::is_none) {
        w.warn("text styles that could not be read (the style table or defaults are used)");
    }
    if st.object_styles.len() > 1 || st.char_styles.len() > 1 || !st.records_zero {
        w.warn("text with several character styles (drawn with the first; unverified)");
    }

    // Characters: one Windows-1252 byte each is the verified layout; anything else is recovered.
    let raw = if st.text.len() == st.chars && st.records_zero {
        cp1252(st.text)
    } else {
        w.warn("text stored in an unverified layout (characters recovered best effort)");
        match tables.indexed(c, leaf) {
            Some(s) => s,
            None if st.text.len() == 2 * st.chars => utf16le(st.text),
            None => cp1252(st.text),
        }
    };
    let chars = raw.replace("\r\n", "\n").replace('\r', "\n");

    let styles = paint.styles.as_ref();
    let style = Style { object, character, font: style_id.and_then(|id| styles?.font(id)), tables };
    match style_id.and_then(|id| styles?.align(id)) {
        None | Some(0) => {}
        Some(_) => w.warn("text alignments other than left (unverified; left used)"),
    }
    let spacing = style
        .get(&["paragraph", "interCharSpacing"])
        .and_then(Value::as_f64)
        .or_else(|| style_id.and_then(|id| styles?.char_spacing(id)).map(f64::from))
        .unwrap_or(0.0);
    let tracking = (spacing / 1e6 * SPACE_EM).clamp(-10.0, 10.0);

    // Paint: a character-style colour overrides the object's fill chain.
    let override_fill = character
        .and_then(|v| v.path(&["character", "fill"]))
        .filter(|f| f.get("type").and_then(Value::as_f64) == Some(1.0))
        .and_then(|f| f.get("primaryColor")?.as_str())
        .and_then(paint::color_string);
    let fill = match override_fill {
        Some(color) => Paint::Solid(color),
        None if t.loda.arg(arg::FILL).is_some() => paint.fill(t, w),
        None => match style_id.and_then(|id| styles?.fill(id)) {
            Some(id) => paint.fill_by_id(id, t, w),
            None => {
                let json_color = object.and_then(|v| v.path(&["character", "fill", "primaryColor"])?.as_str()).and_then(paint::color_string);
                Paint::Solid(json_color.unwrap_or(Color::Rgb { r: 0.0, g: 0.0, b: 0.0 }))
            }
        },
    };
    let outline = if t.loda.arg(arg::OUTLINE).is_some() {
        paint.outline(t, w)
    } else {
        style_id.and_then(|id| styles?.outline(id)).and_then(|id| paint.outline_by_id(id, t, w))
    };

    let latin = style.face(0, None);
    let faces = [latin.clone(), style.face(1, Some(&latin)), style.face(2, Some(&latin))];
    let mut pieces: Vec<(usize, String)> = Vec::new();
    for ch in chars.chars() {
        let slot = slot_of(ch, pieces.last().map_or(0, |p| p.0));
        match pieces.last_mut() {
            Some((s, text)) if *s == slot => text.push(ch),
            _ => pieces.push((slot, ch.to_string())),
        }
    }
    for (slot, text) in pieces {
        let Some(face) = faces.get(slot) else { continue };
        let mut size_pt = face.size * POINTS_PER_UNIT * scale;
        if !size_pt.is_finite() || size_pt <= 0.0 {
            w.warn("text of an unusable size (12 pt used)");
            size_pt = 12.0;
        }
        out.runs.push(Run {
            text,
            family: face.family.clone(),
            postscript_name: face.postscript.clone(),
            size_pt,
            weight: face.weight.map_or(400, |b| css_weight(b, w)),
            italic: face.italic,
            underline: face.underline,
            tracking,
            fill: fill.clone(),
            outline: outline.clone(),
        });
    }
    out
}
