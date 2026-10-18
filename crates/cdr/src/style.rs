//! The document style table (`doc/stlt`): text styles and the records they are built from.
//!
//! ```text
//! u32 nStyles
//! 7 sections, each u32 count then count fixed-size records starting {u32 id, u32 parent}:
//!   fill 60 B       + u32 fillId, f64[6] fill matrix
//!   outline 12 B    + u32 outlineId
//!   font 60 B       + u32[4], u32 fontId[3], u32 size[3] (world units), u32 weight[3]   (latin, far-east, right-to-left)
//!   align 12 B      + u32 value (0 = left; nothing else seen)
//!   interval 52 B   + u32 mask, i32 interChar, interLang, interWord, interLine, beforePara, afterPara, … (×1e6)
//!   decoration 172 B, misc 12 B
//! nStyles × 52 B: u32 kind, u32 styleId, u32 parent, u32[3], u32 refs[7] (one record id per section, 0 = none)
//! ```
//!
//! Record ids share one id space; style ids are another. The layout was checked by exact
//! consumption on one sample (14 text styles), so a table of any other length is ignored with a
//! warning rather than guessed at.

use std::collections::HashMap;

use crate::container::Container;
use crate::geometry::{i32_at, u32_at};
use crate::model::Warnings;

const MAX_RECORDS: usize = 100_000;
/// Record sizes of the seven sections, in file order.
const SECTION_SIZES: [usize; 7] = [60, 12, 60, 12, 52, 172, 12];
const STYLE_SIZE: usize = 52;
/// Parent links followed when a style leaves a section to its parent.
const MAX_PARENTS: usize = 16;

const FILL: usize = 0;
const OUTLINE: usize = 1;
const FONT: usize = 2;
const ALIGN: usize = 3;
const INTERVAL: usize = 4;

/// The font part of a text style: latin, far-east and right-to-left slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FontRecord {
    pub(crate) font_ids: [u32; 3],
    /// Em sizes in world units.
    pub(crate) sizes: [u32; 3],
    /// Weight bit fields (see `text::css_weight`).
    pub(crate) weights: [u32; 3],
}

#[derive(Debug, Clone, PartialEq)]
enum Record {
    Fill(u32),
    Outline(u32),
    Font(FontRecord),
    Align(u32),
    /// Character spacing, ×1e6 of the space width.
    Interval(i32),
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Style {
    parent: u32,
    refs: [u32; 7],
}

#[derive(Debug, Default)]
pub(crate) struct Styles {
    records: HashMap<u32, (usize, Record)>,
    styles: HashMap<u32, Style>,
}

impl Styles {
    /// Decode `doc/stlt`, or `None` (with a warning) when it is absent or not laid out as expected.
    pub(crate) fn load(c: &Container<'_>, doc: usize, w: &mut Warnings) -> Option<Self> {
        let payload = c.tree.child_leaf(doc, b"stlt").map(|i| c.payload(i))?;
        match payload.map_err(|_| "a damaged style table").and_then(Self::decode) {
            Ok(s) => Some(s),
            Err(e) => {
                w.warn(format!("the style table ({e}; text styles fall back to defaults)"));
                None
            }
        }
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self, &'static str> {
        const SHORT: &str = "truncated";
        let n_styles = u32_at(b, 0).ok_or(SHORT)? as usize;
        if n_styles > MAX_RECORDS {
            return Err("too many styles");
        }
        let mut at = 4usize;
        let mut records = HashMap::new();
        for (section, size) in SECTION_SIZES.into_iter().enumerate() {
            let count = u32_at(b, at).ok_or(SHORT)? as usize;
            if count > MAX_RECORDS {
                return Err("too many style records");
            }
            at += 4;
            let end = count.checked_mul(size).and_then(|n| n.checked_add(at)).filter(|e| *e <= b.len()).ok_or(SHORT)?;
            for k in 0..count {
                let r = b.get(at + k * size..at + (k + 1) * size).ok_or(SHORT)?;
                let id = u32_at(r, 0).ok_or(SHORT)?;
                let u = |o: usize| u32_at(r, o).ok_or(SHORT);
                let record = match section {
                    FILL => Record::Fill(u(8)?),
                    OUTLINE => Record::Outline(u(8)?),
                    FONT => Record::Font(FontRecord {
                        font_ids: [u(24)?, u(28)?, u(32)?],
                        sizes: [u(36)?, u(40)?, u(44)?],
                        weights: [u(48)?, u(52)?, u(56)?],
                    }),
                    ALIGN => Record::Align(u(8)?),
                    INTERVAL => Record::Interval(i32_at(r, 12).ok_or(SHORT)?),
                    _ => Record::Other,
                };
                records.entry(id).or_insert((section, record));
            }
            at = end;
        }
        let end = n_styles.checked_mul(STYLE_SIZE).and_then(|n| n.checked_add(at)).ok_or(SHORT)?;
        if end != b.len() {
            return Err("an unfamiliar layout");
        }
        let mut styles = HashMap::new();
        for k in 0..n_styles {
            let r = b.get(at + k * STYLE_SIZE..at + (k + 1) * STYLE_SIZE).ok_or(SHORT)?;
            let id = u32_at(r, 4).ok_or(SHORT)?;
            let mut refs = [0u32; 7];
            for (j, slot) in refs.iter_mut().enumerate() {
                *slot = u32_at(r, 24 + 4 * j).ok_or(SHORT)?;
            }
            styles.entry(id).or_insert(Style { parent: u32_at(r, 8).ok_or(SHORT)?, refs });
        }
        Ok(Self { records, styles })
    }

    /// The record a style uses for one section, inherited from its parents when it names none.
    fn record(&self, style: u32, section: usize) -> Option<&Record> {
        let mut id = style;
        for _ in 0..MAX_PARENTS {
            let s = self.styles.get(&id)?;
            let r = s.refs.get(section).copied().unwrap_or(0);
            if r != 0 {
                return self.records.get(&r).filter(|(sec, _)| *sec == section).map(|(_, rec)| rec);
            }
            if s.parent == 0 || s.parent == id {
                return None;
            }
            id = s.parent;
        }
        None
    }

    pub(crate) fn fill(&self, style: u32) -> Option<u32> {
        match self.record(style, FILL)? {
            Record::Fill(id) => Some(*id),
            _ => None,
        }
    }

    pub(crate) fn outline(&self, style: u32) -> Option<u32> {
        match self.record(style, OUTLINE)? {
            Record::Outline(id) => Some(*id),
            _ => None,
        }
    }

    pub(crate) fn font(&self, style: u32) -> Option<FontRecord> {
        match self.record(style, FONT)? {
            Record::Font(f) => Some(*f),
            _ => None,
        }
    }

    pub(crate) fn align(&self, style: u32) -> Option<u32> {
        match self.record(style, ALIGN)? {
            Record::Align(v) => Some(*v),
            _ => None,
        }
    }

    /// Character spacing ×1e6 of the space width.
    pub(crate) fn char_spacing(&self, style: u32) -> Option<i32> {
        match self.record(style, INTERVAL)? {
            Record::Interval(v) => Some(*v),
            _ => None,
        }
    }
}
