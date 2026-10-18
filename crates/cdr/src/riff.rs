//! The RIFF chunk tree of a CorelDRAW stream (`content/root.dat`, or a whole legacy file).
//!
//! ```text
//! RIFF chunk: "RIFF" | u32 size | form type (e.g. "CDRT") | children…   (size counts the form type)
//! LIST chunk: "LIST" | u32 size | list type (e.g. "page") | children…
//! leaf chunk: id[4]  | u32 size | size bytes of body
//! ```
//!
//! The walk is iterative with an explicit stack, every chunk must fit inside its parent, an odd
//! body is followed by one pad byte, and the chunk count and depth are capped. The tree only
//! records where each body is; what a leaf body means (a 16-byte reference into a data file, or
//! the data itself in older layouts) is decided by the container.

use crate::Error;

/// Most chunks a stream may hold (the sample has 28 594).
pub const MAX_CHUNKS: usize = 1_000_000;
/// Deepest list nesting followed (the sample reaches 16). The walk is iterative, so this bounds
/// memory, not the call stack.
pub const MAX_CHUNK_DEPTH: usize = 256;

const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// A leaf's id, or the list type of a `RIFF`/`LIST` chunk (four Latin-1 bytes, space padded).
    pub id: [u8; 4],
    pub list: bool,
    /// Offset of the body in the stream: a leaf's data, or a list's first child.
    pub start: usize,
    /// Body length: a leaf's data, or all of a list's children.
    pub len: usize,
    /// 0 for the root.
    pub depth: usize,
    first: u32,
    next: u32,
}

/// A parsed chunk tree. Index 0 is the root `RIFF` chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    chunks: Vec<Chunk>,
    /// Bytes inside lists that were too short to be a chunk, plus bytes after the root chunk.
    pub stray_bytes: usize,
}

struct Frame {
    index: usize,
    end: usize,
    /// Where the next sibling of this list starts (after its pad byte).
    after: usize,
    last: u32,
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn id_at(b: &[u8], at: usize) -> Option<[u8; 4]> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some([s[0], s[1], s[2], s[3]])
}

fn index(i: usize) -> Result<u32, Error> {
    u32::try_from(i).map_err(|_| Error::Limit("too many chunks"))
}

impl Tree {
    /// Parse a stream that starts with a `RIFF` chunk.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if !bytes.starts_with(b"RIFF") {
            return Err(Error::Malformed("chunk stream does not start with RIFF"));
        }
        let size = le32(bytes, 4).ok_or(Error::Malformed("truncated chunk stream"))?;
        let size = usize::try_from(size).map_err(|_| Error::Malformed("chunk size"))?;
        if size < 4 {
            return Err(Error::Malformed("RIFF chunk too short"));
        }
        let end = size.checked_add(8).filter(|e| *e <= bytes.len()).ok_or(Error::Malformed("chunk stream is truncated"))?;
        let form = id_at(bytes, 8).ok_or(Error::Malformed("truncated chunk stream"))?;
        let mut chunks = vec![Chunk { id: form, list: true, start: 12, len: size - 4, depth: 0, first: NONE, next: NONE }];
        let mut stray_bytes = bytes.len() - end;
        let mut stack = vec![Frame { index: 0, end, after: end, last: NONE }];
        let mut pos = 12usize;
        while let Some(top) = stack.last() {
            let (parent, parent_end) = (top.index, top.end);
            let left = parent_end.saturating_sub(pos);
            if left < 8 {
                stray_bytes += left;
                pos = top.after;
                stack.pop();
                continue;
            }
            let id = id_at(bytes, pos).ok_or(Error::Malformed("truncated chunk stream"))?;
            let size = le32(bytes, pos + 4).ok_or(Error::Malformed("truncated chunk stream"))?;
            let size = usize::try_from(size).map_err(|_| Error::Malformed("chunk size"))?;
            let body = pos + 8;
            let body_end = body.checked_add(size).filter(|e| *e <= parent_end).ok_or(Error::Malformed("a chunk overruns its parent"))?;
            let after = body_end.saturating_add(size & 1).min(parent_end);
            let i = chunks.len();
            if i >= MAX_CHUNKS {
                return Err(Error::Limit("too many chunks"));
            }
            let link = index(i)?;
            match top.last {
                NONE => {
                    if let Some(p) = chunks.get_mut(parent) {
                        p.first = link;
                    }
                }
                last => {
                    if let Some(p) = chunks.get_mut(last as usize) {
                        p.next = link;
                    }
                }
            }
            if let Some(top) = stack.last_mut() {
                top.last = link;
            }
            let depth = stack.len();
            if &id == b"RIFF" || &id == b"LIST" {
                if size < 4 {
                    return Err(Error::Malformed("list chunk too short"));
                }
                if depth > MAX_CHUNK_DEPTH {
                    return Err(Error::Limit("chunks nested too deeply"));
                }
                let kind = id_at(bytes, body).ok_or(Error::Malformed("truncated chunk stream"))?;
                chunks.push(Chunk { id: kind, list: true, start: body + 4, len: size - 4, depth, first: NONE, next: NONE });
                stack.push(Frame { index: i, end: body_end, after, last: NONE });
                pos = body + 4;
            } else {
                chunks.push(Chunk { id, list: false, start: body, len: size, depth, first: NONE, next: NONE });
                pos = after;
            }
        }
        Ok(Self { chunks, stray_bytes })
    }

    pub fn root(&self) -> usize {
        0
    }

    pub fn get(&self, i: usize) -> Option<&Chunk> {
        self.chunks.get(i)
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Every chunk in pre-order (file order).
    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// The children of chunk `i` in file order.
    pub fn children(&self, i: usize) -> Children<'_> {
        Children { tree: self, next: self.chunks.get(i).map_or(NONE, |c| c.first) }
    }

    /// The first child of `i` with this id (leaf id or list type).
    pub fn child(&self, i: usize, id: &[u8; 4]) -> Option<usize> {
        self.children(i).find(|c| self.chunks.get(*c).is_some_and(|c| &c.id == id))
    }

    /// The first child list of `i` with this list type.
    pub fn child_list(&self, i: usize, id: &[u8; 4]) -> Option<usize> {
        self.children(i).find(|c| self.chunks.get(*c).is_some_and(|c| c.list && &c.id == id))
    }

    /// The first child leaf of `i` with this id.
    pub fn child_leaf(&self, i: usize, id: &[u8; 4]) -> Option<usize> {
        self.children(i).find(|c| self.chunks.get(*c).is_some_and(|c| !c.list && &c.id == id))
    }

    /// A chunk's raw body in `bytes` (the stream the tree was parsed from).
    pub fn body<'b>(&self, bytes: &'b [u8], i: usize) -> Option<&'b [u8]> {
        let c = self.chunks.get(i)?;
        bytes.get(c.start..c.start.checked_add(c.len)?)
    }
}

/// Iterator over the children of one chunk. Links only point forward, so it always ends.
pub struct Children<'t> {
    tree: &'t Tree,
    next: u32,
}

impl Iterator for Children<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.next == NONE {
            return None;
        }
        let i = self.next as usize;
        self.next = self.tree.chunks.get(i).map_or(NONE, |c| c.next);
        Some(i)
    }
}
