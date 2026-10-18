//! A bounded, read-only ZIP reader: the central directory is authoritative for names, sizes,
//! checksums and offsets; the local header is used only to find where an entry's data starts.
//! Stored and deflated entries are supported; encrypted, multi-volume and ZIP64 archives are
//! refused. Every size is checked against [`Limits`] before anything is allocated, and every entry
//! must decode to exactly its declared size and CRC-32.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::Read;

use crate::{Error, Limits};

/// Most entries an archive may list (CorelDRAW documents have a few dozen).
pub const MAX_ENTRIES: usize = 10_000;

const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const END_SIGNATURE: u32 = 0x0605_4b50;
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const END_SIZE: usize = 22;
const LOCAL_SIZE: usize = 30;
const CENTRAL_SIZE: usize = 46;

/// What the first local header of a file says, read from the head alone (for content sniffing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstEntry<'a> {
    pub name: &'a [u8],
    pub method: u16,
    /// The start of the entry's stored bytes, as far as the head reaches.
    pub data_prefix: &'a [u8],
}

/// The first local header of a ZIP file, or `None` when `bytes` do not start with one.
pub fn first_local_entry(bytes: &[u8]) -> Option<FirstEntry<'_>> {
    if le32(bytes, 0) != Some(LOCAL_SIGNATURE) {
        return None;
    }
    let method = le16(bytes, 8)?;
    let compressed = usize::try_from(le32(bytes, 18)?).ok()?;
    let name_len = usize::from(le16(bytes, 26)?);
    let extra_len = usize::from(le16(bytes, 28)?);
    let name = bytes.get(LOCAL_SIZE..LOCAL_SIZE.checked_add(name_len)?)?;
    let start = LOCAL_SIZE.checked_add(name_len)?.checked_add(extra_len)?;
    let end = start.saturating_add(compressed).min(bytes.len());
    let data_prefix = bytes.get(start..end).unwrap_or_default();
    Some(FirstEntry { name, method, data_prefix })
}

/// One central-directory record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// 0 = stored, 8 = deflate; anything else is refused when the entry is read.
    pub method: u16,
    pub flags: u16,
    pub crc: u32,
    pub compressed: u64,
    pub size: u64,
    local_offset: u64,
}

/// An opened archive. Reading takes `&self`, so payloads of several entries can be borrowed at
/// once; the extraction budget is tracked in a cell.
#[derive(Debug)]
pub struct Zip<'a> {
    bytes: &'a [u8],
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    /// Start of the central directory: entry data must end before it.
    directory: usize,
    extracted: Cell<usize>,
    limits: Limits,
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn truncated<T>(v: Option<T>) -> Result<T, Error> {
    v.ok_or(Error::Malformed("truncated ZIP directory"))
}

/// The end-of-central-directory record: the last one whose comment ends exactly at the end of the file.
fn find_end(bytes: &[u8]) -> Result<usize, Error> {
    let Some(mut pos) = bytes.len().checked_sub(END_SIZE) else {
        return Err(Error::Malformed("not a ZIP file"));
    };
    let lowest = pos.saturating_sub(0xFFFF);
    loop {
        if le32(bytes, pos) == Some(END_SIGNATURE) {
            let comment = le16(bytes, pos + 20).map_or(usize::MAX, usize::from);
            if pos.checked_add(END_SIZE).and_then(|e| e.checked_add(comment)) == Some(bytes.len()) {
                return Ok(pos);
            }
        }
        if pos == lowest {
            return Err(Error::Malformed("ZIP end record not found"));
        }
        pos -= 1;
    }
}

/// True when an extra field holds a ZIP64 record (id 1). A malformed extra field is ignored.
fn has_zip64_extra(mut extra: &[u8]) -> bool {
    while extra.len() >= 4 {
        let id = u16::from_le_bytes([extra[0], extra[1]]);
        let len = usize::from(u16::from_le_bytes([extra[2], extra[3]]));
        if id == 1 {
            return true;
        }
        let Some(rest) = extra.get(4 + len..) else { return false };
        extra = rest;
    }
    false
}

impl<'a> Zip<'a> {
    pub fn open(bytes: &'a [u8], limits: Limits) -> Result<Self, Error> {
        let end = find_end(bytes)?;
        if end >= 20 && le32(bytes, end - 20) == Some(ZIP64_LOCATOR_SIGNATURE) {
            return Err(Error::Unsupported("ZIP64 archives"));
        }
        let disk = truncated(le16(bytes, end + 4))?;
        let directory_disk = truncated(le16(bytes, end + 6))?;
        let on_disk = truncated(le16(bytes, end + 8))?;
        let count = truncated(le16(bytes, end + 10))?;
        let directory_size = truncated(le32(bytes, end + 12))?;
        let directory_offset = truncated(le32(bytes, end + 16))?;
        if count == 0xFFFF || directory_size == u32::MAX || directory_offset == u32::MAX {
            return Err(Error::Unsupported("ZIP64 archives"));
        }
        if disk != 0 || directory_disk != 0 || on_disk != count {
            return Err(Error::Unsupported("multi-volume ZIP archives"));
        }
        let count = usize::from(count);
        if count > MAX_ENTRIES {
            return Err(Error::Limit("too many ZIP entries"));
        }
        let start = usize::try_from(directory_offset).map_err(|_| Error::Malformed("ZIP directory offset"))?;
        let size = usize::try_from(directory_size).map_err(|_| Error::Malformed("ZIP directory size"))?;
        let stop = start.checked_add(size).filter(|s| *s <= end).ok_or(Error::Malformed("ZIP central directory outside the file"))?;
        let mut entries = Vec::with_capacity(count);
        let mut index = HashMap::with_capacity(count);
        let mut pos = start;
        for _ in 0..count {
            let header_end = pos.checked_add(CENTRAL_SIZE).filter(|e| *e <= stop).ok_or(Error::Malformed("truncated ZIP directory"))?;
            if le32(bytes, pos) != Some(CENTRAL_SIGNATURE) {
                return Err(Error::Malformed("ZIP central directory record missing"));
            }
            let flags = truncated(le16(bytes, pos + 8))?;
            let method = truncated(le16(bytes, pos + 10))?;
            let crc = truncated(le32(bytes, pos + 16))?;
            let compressed = truncated(le32(bytes, pos + 20))?;
            let size = truncated(le32(bytes, pos + 24))?;
            let name_len = usize::from(truncated(le16(bytes, pos + 28))?);
            let extra_len = usize::from(truncated(le16(bytes, pos + 30))?);
            let comment_len = usize::from(truncated(le16(bytes, pos + 32))?);
            let local = truncated(le32(bytes, pos + 42))?;
            let name_end = header_end.checked_add(name_len).ok_or(Error::Malformed("truncated ZIP directory"))?;
            let extra_end = name_end.checked_add(extra_len).ok_or(Error::Malformed("truncated ZIP directory"))?;
            let next = extra_end.checked_add(comment_len).filter(|n| *n <= stop).ok_or(Error::Malformed("truncated ZIP directory"))?;
            let name = truncated(bytes.get(header_end..name_end))?;
            let extra = truncated(bytes.get(name_end..extra_end))?;
            if compressed == u32::MAX || size == u32::MAX || local == u32::MAX || has_zip64_extra(extra) {
                return Err(Error::Unsupported("ZIP64 archives"));
            }
            // Bit 0: traditional encryption; bit 6: strong encryption.
            if flags & 0x41 != 0 {
                return Err(Error::Unsupported("encrypted ZIP archives"));
            }
            let name = String::from_utf8_lossy(name).into_owned();
            if index.insert(name.clone(), entries.len()).is_some() {
                return Err(Error::Malformed("duplicate ZIP entry names"));
            }
            entries.push(Entry {
                name,
                method,
                flags,
                crc,
                compressed: u64::from(compressed),
                size: u64::from(size),
                local_offset: u64::from(local),
            });
            pos = next;
        }
        Ok(Self { bytes, entries, index, directory: start, extracted: Cell::new(0), limits })
    }

    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.index.get(name).and_then(|i| self.entries.get(*i))
    }

    /// Entries in central-directory order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Bytes extracted so far, counted against [`Limits::max_total`].
    pub fn extracted(&self) -> usize {
        self.extracted.get()
    }

    /// Extract an entry, refusing it before allocating when it declares more than `cap` bytes.
    pub fn read_capped(&self, name: &str, cap: usize, what: &'static str) -> Result<Vec<u8>, Error> {
        let e = self.entry(name).ok_or(Error::Malformed("missing ZIP entry"))?;
        if e.size > u64::try_from(cap).unwrap_or(u64::MAX) {
            return Err(Error::Limit(what));
        }
        self.read(name)
    }

    /// Extract an entry by name: bounded decompression, exact size and CRC-32.
    pub fn read(&self, name: &str) -> Result<Vec<u8>, Error> {
        let e = self.entry(name).ok_or(Error::Malformed("missing ZIP entry"))?;
        let size = usize::try_from(e.size).map_err(|_| Error::Limit("ZIP entry too large"))?;
        if size > self.limits.max_entry {
            return Err(Error::Limit("ZIP entry too large"));
        }
        let total = self.extracted.get().checked_add(size).ok_or(Error::Limit("document too large"))?;
        if total > self.limits.max_total {
            return Err(Error::Limit("document too large"));
        }
        if e.method != 0 && e.method != 8 {
            return Err(Error::Unsupported("ZIP compression method"));
        }
        let local = usize::try_from(e.local_offset).map_err(|_| Error::Malformed("ZIP entry outside the file"))?;
        let header = local
            .checked_add(LOCAL_SIZE)
            .filter(|h| *h <= self.directory)
            .and_then(|h| self.bytes.get(local..h))
            .ok_or(Error::Malformed("ZIP entry outside the file"))?;
        if le32(header, 0) != Some(LOCAL_SIGNATURE) {
            return Err(Error::Malformed("ZIP local header missing"));
        }
        let name_len = usize::from(le16(header, 26).ok_or(Error::Malformed("truncated ZIP entry"))?);
        let extra_len = usize::from(le16(header, 28).ok_or(Error::Malformed("truncated ZIP entry"))?);
        let compressed = usize::try_from(e.compressed).map_err(|_| Error::Malformed("ZIP entry outside the file"))?;
        let start = local
            .checked_add(LOCAL_SIZE)
            .and_then(|s| s.checked_add(name_len))
            .and_then(|s| s.checked_add(extra_len))
            .ok_or(Error::Malformed("ZIP entry outside the file"))?;
        let stop = start.checked_add(compressed).filter(|s| *s <= self.directory).ok_or(Error::Malformed("ZIP entry outside the file"))?;
        let data = self.bytes.get(start..stop).ok_or(Error::Malformed("ZIP entry outside the file"))?;
        let out = if e.method == 0 {
            if compressed != size {
                return Err(Error::Malformed("ZIP entry size mismatch"));
            }
            data.to_vec()
        } else {
            read_bounded(flate2::read::DeflateDecoder::new(data), size)?
        };
        if out.len() != size {
            return Err(Error::Malformed("ZIP entry size mismatch"));
        }
        if crc32fast::hash(&out) != e.crc {
            return Err(Error::Malformed("ZIP entry checksum mismatch"));
        }
        self.extracted.set(total);
        Ok(out)
    }
}

/// Read at most `size + 1` bytes so a stream that decodes past its declared size is caught
/// without unbounded allocation.
pub(crate) fn read_bounded(mut r: impl Read, size: usize) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(size.min(64 << 20));
    let cap = u64::try_from(size).unwrap_or(u64::MAX).saturating_add(1);
    r.by_ref().take(cap).read_to_end(&mut out).map_err(|_| Error::Malformed("corrupt compressed ZIP entry"))?;
    Ok(out)
}
