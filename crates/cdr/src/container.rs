//! The container: a CorelDRAW ZIP, its main chunk stream (`content/root.dat`) and the payload of
//! every leaf chunk.
//!
//! In the "split" layout every leaf body is a 16-byte reference:
//!
//! ```text
//! i32 fileIndex  -1: the payload is inline, bytes 8..8+length of this body (length <= 8)
//!                >= 0: line `fileIndex` of content/dataFileList.dat names content/data/<name>
//! u32 length
//! u32 offset     (fileIndex >= 0) byte offset of the payload in that data file
//! u32 reserved   (fileIndex >= 0) 0
//! ```
//!
//! Older layouts keep the payload in the leaf body itself ([`Layout::Inline`]). They are
//! recognised but refused for now: nothing verifies how their records are laid out.

use std::cell::OnceCell;

use crate::Error;
use crate::riff::Tree;
use crate::zip::Zip;

/// Decompression and decoding budget for one import. Sizes are checked before anything is
/// allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest single ZIP entry (`content/data/Bitmaps.dat` is 27.6 MB in the sample).
    pub max_entry: usize,
    /// Total bytes extracted from the ZIP over the whole import.
    pub max_total: usize,
    /// Bitmap pixels decoded over the whole document.
    pub max_pixels: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_entry: 256 << 20, max_total: 1 << 30, max_pixels: 512 << 20 }
    }
}

/// Most data files `content/dataFileList.dat` may name (the sample has 4).
pub(crate) const MAX_DATA_FILES: usize = 4096;
/// Small text entries (`META-INF/container.xml`, `content/dataFileList.dat`).
const MAX_SMALL_ENTRY: usize = 64 << 10;
/// Longest inline payload of a split-layout reference.
const MAX_INLINE: usize = 8;

const ROOT_STREAM: &str = "content/root.dat";
const OLDER_ZIP_STREAM: &str = "content/riffData.cdr";
const OLDER_ZIP: &str = "older ZIP layout (content/riffData.cdr), not supported yet";
const OLDER_RIFF: &str = "older bare-RIFF layout (before the ZIP container), not supported yet";
const INLINE_RECORDS: &str = "records stored inline (an older layout), not supported yet";

/// How leaf bodies hold their payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    /// 16-byte references into `content/data/*.dat` (format version 2600).
    Split,
    /// The body is the payload (older layouts; not enabled yet).
    #[allow(dead_code)]
    Inline,
}

#[derive(Debug)]
struct DataFile {
    name: String,
    bytes: OnceCell<Result<Vec<u8>, Error>>,
}

#[derive(Debug)]
pub(crate) struct Container<'a> {
    zip: Zip<'a>,
    stream: Vec<u8>,
    pub(crate) tree: Tree,
    layout: Layout,
    files: Vec<DataFile>,
}

impl<'a> Container<'a> {
    pub(crate) fn open(bytes: &'a [u8], limits: Limits) -> Result<Self, Error> {
        if bytes.starts_with(b"RIFF") {
            if bytes.get(8..11) == Some(b"CDR".as_slice()) {
                return Err(Error::Unsupported(OLDER_RIFF));
            }
            return Err(Error::Malformed("not a CorelDRAW file"));
        }
        if !bytes.starts_with(b"PK") {
            return Err(Error::Malformed("not a CorelDRAW file"));
        }
        let zip = Zip::open(bytes, limits)?;
        let main = main_stream(&zip);
        let path = main.as_deref().filter(|p| zip.entry(p).is_some()).unwrap_or(ROOT_STREAM);
        if path.ends_with("riffData.cdr") || (zip.entry(path).is_none() && zip.entry(OLDER_ZIP_STREAM).is_some()) {
            return Err(Error::Unsupported(OLDER_ZIP));
        }
        if zip.entry(path).is_none() {
            return Err(Error::Unsupported("the ZIP archive holds no CorelDRAW document"));
        }
        let stream = zip.read(path)?;
        let tree = Tree::parse(&stream)?;
        if tree.get(0).is_none_or(|r| !r.id.starts_with(b"CDR")) {
            return Err(Error::Malformed("the document stream is not a CorelDRAW chunk tree"));
        }
        if tree.chunks().iter().any(|c| !c.list && c.len != 16) {
            return Err(Error::Unsupported(INLINE_RECORDS));
        }
        let files = match zip.entry("content/dataFileList.dat") {
            None => Vec::new(),
            Some(_) => data_file_names(&zip.read_capped("content/dataFileList.dat", MAX_SMALL_ENTRY, "data file list too long")?)?,
        };
        Ok(Self { zip, stream, tree, layout: Layout::Split, files })
    }

    /// A small side entry of the archive outside the chunk tree (`font/fontTable.dat`,
    /// `color/docPalette.xml`, `META-INF/textinfo.xml`): `None` when it is absent, too large or
    /// damaged (side entries are optional, so their failures only cost what they would have added).
    pub(crate) fn side_entry(&self, name: &str, cap: usize) -> Option<Vec<u8>> {
        self.zip.entry(name)?;
        self.zip.read_capped(name, cap, "side entry too long").ok()
    }

    /// The resolved payload of leaf chunk `i`.
    pub(crate) fn payload(&self, i: usize) -> Result<&[u8], Error> {
        let chunk = self.tree.get(i).filter(|c| !c.list).ok_or(Error::Malformed("not a record"))?;
        let body = self.tree.body(&self.stream, i).ok_or(Error::Malformed("record outside the stream"))?;
        match self.layout {
            Layout::Inline => Ok(body),
            Layout::Split => {
                let field = |at: usize| body.get(at..at + 4).map(|s| [s[0], s[1], s[2], s[3]]);
                let (Some(file), Some(len), Some(offset), 16) = (field(0), field(4), field(8), chunk.len) else {
                    return Err(Error::Malformed("bad record reference"));
                };
                let (file, len, offset) = (i32::from_le_bytes(file), u32::from_le_bytes(len) as usize, u32::from_le_bytes(offset) as usize);
                if file == -1 {
                    if len > MAX_INLINE {
                        return Err(Error::Malformed("inline record too long"));
                    }
                    return body.get(8..8 + len).ok_or(Error::Malformed("bad record reference"));
                }
                let file = usize::try_from(file).map_err(|_| Error::Malformed("bad record reference"))?;
                let data = self.data_file(file)?;
                let end = offset.checked_add(len).ok_or(Error::Malformed("record reference outside its data file"))?;
                data.get(offset..end).ok_or(Error::Malformed("record reference outside its data file"))
            }
        }
    }

    /// The bytes of data file `i`, extracted on first use (and the failure remembered).
    fn data_file(&self, i: usize) -> Result<&[u8], Error> {
        let f = self.files.get(i).ok_or(Error::Malformed("record refers to a missing data file"))?;
        let cell = f.bytes.get_or_init(|| {
            let path = format!("content/data/{}", f.name);
            if self.zip.entry(&path).is_none() {
                return Err(Error::Malformed("missing data file"));
            }
            self.zip.read(&path)
        });
        match cell {
            Ok(v) => Ok(v),
            Err(e) => Err(*e),
        }
    }

    /// The format version: `vrsn` (u16), else the first u16 of `fver`.
    pub(crate) fn version(&self) -> Option<u32> {
        let root = self.tree.root();
        if let Some([a, b]) = self.tree.child_leaf(root, b"vrsn").and_then(|i| self.payload(i).ok()) {
            return Some(u32::from(u16::from_le_bytes([*a, *b])));
        }
        let f = self.tree.child_leaf(root, b"fver").and_then(|i| self.payload(i).ok())?;
        let s = f.get(..2)?;
        Some(u32::from(u16::from_le_bytes([s[0], s[1]])))
    }
}

/// The format version of a CorelDRAW document (2600 for the files this reader was checked on),
/// read without building the document model.
pub fn version(bytes: &[u8]) -> Result<u32, Error> {
    Container::open(bytes, Limits::default())?.version().ok_or(Error::Malformed("no version record"))
}

/// `META-INF/container.xml`: the `full-path` of the `rootfile` whose `file-kind` is `main`.
/// Optional: a missing, oversized or unparseable file falls back to `content/root.dat`.
fn main_stream(zip: &Zip<'_>) -> Option<String> {
    zip.entry("META-INF/container.xml")?;
    let xml = zip.read_capped("META-INF/container.xml", MAX_SMALL_ENTRY, "container.xml too long").ok()?;
    let xml = String::from_utf8_lossy(&xml);
    let mut rest = xml.as_ref();
    while let Some(at) = rest.find("<rootfile") {
        rest = rest.get(at + "<rootfile".len()..)?;
        let end = rest.find('>')?;
        let attrs = rest.get(..end)?;
        if attribute(attrs, "file-kind") == Some("main") {
            return attribute(attrs, "full-path").filter(|p| !p.contains('&')).map(str::to_string);
        }
        rest = rest.get(end..)?;
    }
    None
}

/// XML character data with its five named entities and numeric references resolved (an unknown
/// or malformed reference is kept as written).
pub(crate) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let tail = rest.get(at..).unwrap_or_default();
        let entity = tail.find(';').filter(|e| *e <= 12).and_then(|e| tail.get(1..e).map(|name| (name, e)));
        let decoded = entity.and_then(|(name, e)| {
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                _ => {
                    let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => name.strip_prefix('#')?.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = tail.get(e + 1..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = tail.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
    out
}

/// A tolerant attribute scanner: `[prefix:]name = "value"` (or single quotes), any order.
pub(crate) fn attribute<'x>(attrs: &'x str, name: &str) -> Option<&'x str> {
    let mut rest = attrs;
    loop {
        let eq = rest.find('=')?;
        let key = rest.get(..eq)?.trim();
        let key = key.rsplit(|c: char| c.is_whitespace()).next().unwrap_or(key);
        let local = key.rsplit(':').next().unwrap_or(key);
        let after = rest.get(eq + 1..)?.trim_start();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let value_and_rest = after.get(1..)?;
        let close = value_and_rest.find(quote)?;
        let value = value_and_rest.get(..close)?;
        if local == name {
            return Some(value);
        }
        rest = value_and_rest.get(close + 1..)?;
    }
}

/// `content/dataFileList.dat`: one bare file name per line; line i is fileIndex i.
fn data_file_names(bytes: &[u8]) -> Result<Vec<DataFile>, Error> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let name = line.strip_suffix('\r').unwrap_or(line);
        if name.is_empty() {
            continue;
        }
        if name.contains(['/', '\\']) || name.contains("..") || name.chars().any(char::is_control) {
            return Err(Error::Malformed("bad data file name"));
        }
        if out.len() >= MAX_DATA_FILES {
            return Err(Error::Limit("too many data files"));
        }
        out.push(DataFile { name: name.to_string(), bytes: OnceCell::new() });
    }
    Ok(out)
}
