# vectorcraft-cdr

An independent, bounded Rust reader for native CorelDRAW documents (`.cdr`). It reads the ZIP
container of recent CorelDRAW versions, the RIFF chunk tree in `content/root.dat` with its payloads
in `content/data/*.dat` (the "split" layout of format version 2600), and the part of the document
model listed below, and hands the engine pages in points with y pointing down and every transform
composed. It does not write CorelDRAW files.

Older layouts are recognised but not read: ZIPs that keep one `content/riffData.cdr` stream (the X4
to X8 era) and bare `RIFF….CDR?` files from before the ZIP container. For those, and for any split
document so foreign that most of its objects can't be decoded, `read` returns
`Error::Unsupported` so the caller can try another importer or the embedded preview.

## What is imported

| CorelDRAW | Reader model |
|---|---|
| Document pages in file order, page size from the document setup (`mcfg`) | `Page` in points, origin at the page's top-left corner, y down |
| Art layers (normal layers): name, Object Manager colour | `Layer`, bottom to top (the first layer in the file is the front-most) |
| Master-page art layers | repeated on every page, marked `master` |
| Guidelines on the master and page guides layers | `Page::guides` |
| Groups and link groups (the generated part of a contour or blend) | `Kind::Group` |
| Curves: lines, cubic Béziers, open and closed subpaths | `Kind::Shape` paths; even-odd unless the winding argument says nonzero |
| Rectangles, with round corners | paths (four quarter circles) |
| Ellipses | paths (four Bézier quarters, the curve CorelDRAW's own bounds use) |
| PowerClips: the container object and the contents group it names | `Kind::PowerClip { frame, contents }` |
| Uniform fills | `Paint::Solid` |
| Linear and radial fountain fills: colours, positions, midpoints, opacity, angle, centre offset, edge scaling | `Paint::Gradient`, with a gradient-space → page-space transform that keeps CorelDRAW's skew and uneven scaling |
| Colours: RGB, CMYK (0–100 and 0–255 forms), greyscale | `Color::Rgb`, `Color::Cmyk` (kept as CMYK; no colour management), `Color::Gray` |
| Document palette swatches (`color/docPalette.xml`) | `Color::Named` with the swatch and palette names, when a colour carries that palette's id and matches an entry |
| Outlines: width, colour, miter/round/bevel corners, miter limit, "behind fill", "scale with object" | `Outline` in page points (scaled outlines use the object's largest stretch) |
| Uniform transparency | `Node::opacity` |
| Linear and radial transparency ramps | `Node::opacity_mask`, placed like a fountain on the same object |
| Transparency merge modes 0 and 30 | `Blend::Normal`, `Blend::Overlay` (30 is an approximation, reported) |
| Object styles (`stlt`) when an object names one instead of its own paint | the style's fill and outline (unverified, reported) |
| Artistic text: characters (Windows-1252), family, PostScript name, size, weight, italic, underline, character spacing, colour, outline | `Kind::Text` with `Run`s, placed on the first baseline with upright glyphs; a uniform object scale is folded into the size |
| Far-east and right-to-left characters | separate runs in the style's far-east or right-to-left font |
| Bitmaps: 24-bit RGB, 8-bit indexed with a palette, 8-bit alpha masks | `Kind::Image`: straight-alpha RGBA, a pixel → page transform, and `Image::clip` when cropped |
| Mesh fills | filled with the average of their node colours (reported) |
| The bounds CorelDRAW stored for each object | `Node::bounds` |

Every list is reversed on the way in (CorelDRAW stores the front-most child first), and each
object is placed by its own transform only: group transforms are already baked into their
children.

Approximated, each with a warning: merge mode 30 (drawn as overlay; soft light fits the sample
as well), mesh fills (average colour), calligraphic pens (round pen), character spacing (CorelDRAW
measures it in space widths; a space of 0.3 em is assumed), and colours in a model the reader does
not know (taken from the object's own JSON style cache when it names a known model, else black).

Not imported, reported in the warnings one line per kind with a count and never dropped silently:
pattern, texture, PostScript and other fill types (`Paint::Unsupported`), conical and square
fountains (read, but unverified), dashes, arrowheads and line caps (drawn solid with butt caps),
transparency merge modes other than 0 and 30 (drawn normal), pattern transparency, transparency tile
offsets, alignments other than left, paragraph text and multi-style text (the first style is used;
characters in an unverified layout are recovered from the document's text index, or read as UTF-16
or Windows-1252), bitmap pixel formats other than the three above (the crop outline stands in),
outlines on bitmaps, bitmaps beyond the pixel budget, elliptical arcs and pies and non-round
rectangle corners (decoded but unverified), objects of unknown types (drawn from their stored
outline when they have one), objects on the grid or desktop layers, PowerClips whose contents are
missing, shared or contain themselves, objects nested deeper than 128 levels, transforms made of
several parts, unverified format versions, a missing page size (A4 is used) and records that could
not be decoded (skipped; a damaged paint record costs only its own paint). Placeholder objects
without geometry (property carriers from imported files) are not art and are skipped.

## Provenance and clean-room

CorelDRAW's file format has no public specification from its vendor. This reader was written from
a specification worked out by observing the bytes of one document its owner supplied (a recent
CorelDRAW file, format version 2600) and cross-checking every structure inside that file: its
embedded previews, the counts in its own metadata, and references between its records.

* **No CorelDRAW software.** CorelDRAW was never installed, run, scripted, screenshotted or
  disassembled for this work, and no file was made with it for this work.
* **No third-party parser code.** No CorelDRAW reader (libcdr, LibreOffice's import, UniConvertor
  or sK1, Inkscape or its extensions) was read, translated or consulted; no GPL, AGPL or MPL code
  is in this crate.
* **Every structure checked against the file itself.** A layout was adopted only when it decoded
  every instance in the sample, and most also have a semantic check: paths through their own
  transform reproduce the bounds CorelDRAW stored, the page rectangle and the paint order
  reproduce the embedded previews, record counts match the file's metadata, binary paint records
  agree field for field with the JSON copy CorelDRAW caches beside them, fountain placements
  reproduce the cached fill vectors, a render of the decoded colours, gradients and transparency
  matches the embedded thumbnail (mean error under 1/255 per channel), decoded text matches the
  document's text index, and decoded pictures land on the thumbnail's pictures. Statements that
  rest on a single value or none (arc angles, corner order, layer visibility bits, merge modes
  other than 0, conical fountains, older layouts) are marked unverified in the code and produce
  warnings or refusals instead of guesses.
* **The user's file stays private.** It was used locally only. Nothing from it (bytes, text,
  names, pictures, metadata) is in this repository; every test fixture is built in code by the
  `synth` module.
* **Read-only.** Nothing is written in CorelDRAW's format.

## Validation

* Unit tests on synthetic documents built by the `synth` feature: detection (including a 4 KiB
  head and ordinary ZIPs that must not be claimed), every object kind with its transform and the
  y flip, z-order, PowerClip linkage and cycles, master layers, nesting limits, ZIP checksums,
  budgets, encryption and ZIP64 refusals, the older-layout refusals; every colour model, uniform
  and fountain fills with their placement in page space, repaired and unsupported fills, outlines
  (width, scaling, corners, miter angle, behind fill, off pens), transparency and merge modes,
  swatch names, the style-cache and style-table fallbacks, mesh fills; text runs (Windows-1252,
  fonts, sizes under scale and rotation, character-style overrides, script slots, unverified
  layouts, damaged stories); bitmaps (bottom-up rows, stride padding, palettes, masks, crops,
  budgets, unknown formats); the JSON reader; every truncation of a small document, and property
  tests of random input and random mutations of the ZIP, the chunk stream, the data files and the
  side entries.
* `tests/real_files.rs` reads documents named by `VECTORCRAFT_CDR_TEST_FILE` (a `.cdr` file or a
  folder searched recursively) and skips when it is unset. It requires a complete read, compares
  every shape's extents with the bounds CorelDRAW stored, checks that nothing undecoded goes
  unreported, and prints counts only. On the owner's sample: 1 770 objects (1 746 shapes, 14
  texts, 10 images), 1 742 of 1 746 shapes within 2.5 world units (0.25 µm) of their stored bounds
  (three skewed ellipses within 10, one generated mesh outline offset in CorelDRAW's own data);
  1 034 solid fills (17 named swatches), 617 fountains, 228 visible outlines, 620 transparency
  ramps and 20 overlay merges; all 14 texts with their 169 characters; all 10 bitmap objects from
  4 pictures; and only three warning lines (mesh fill approximated, merge modes 29 and 30). Object
  for object, the kinds, paint order, stored bounds, resolved fills (colours, stops, gradient
  transforms), outlines, opacity masks, blend modes, text runs and image pixels and placements
  are identical to those of the independent throwaway decoders the specification was verified
  with.
* `fuzz/` has `cargo-fuzz` targets for the whole reader (`container`) and for the chunk tree,
  record decoders and side entries behind valid ZIP checksums (`riff`):

  ```sh
  cd crates/cdr
  cargo +nightly fuzz run riff -- -max_total_time=600
  ```

What has not been verified: files from other CorelDRAW versions, multi-page documents, several
art layers, master-page art, rectangles with unequal or non-round corners, elliptical arcs and
pies, polygons and stars, paragraph text, spot colours and colour models other than RGB, CMYK and
greyscale, uneven fountain midpoints (the curve), the direction of radial fountains (offset 0 is
taken as the centre), uniform transparency amounts other than 0 %, the glyph shapes of text (the
fonts are not in the file), and anything listed as a warning above. The `synth`
builders write only what this reader accepts; CorelDRAW has never opened their output, so they are
test fixtures, not an exporter.

## Safety limits

Every size is checked before it is allocated: 256 MiB per ZIP entry and 1 GiB extracted per import
by default (`Limits`), and a shared budget of 512 Mi decoded bitmap pixels. The ZIP reader takes
names, sizes, checksums and offsets from the central directory, accepts stored and deflated entries
only, caps the archive at 10 000 entries, refuses encrypted, multi-volume and ZIP64 archives and
duplicate names, requires every entry to lie before the central directory, and checks each
entry's exact size and CRC-32; deflate output is read at most one byte past the declared size.
`META-INF/container.xml` and `content/dataFileList.dat` are capped at 64 KiB, the data file list
at 4096 bare names (no `/`, `\` or `..`), and data files are extracted only when a record needs
them. The chunk tree is walked iteratively with at most 1 000 000 chunks and 256 levels, every chunk
inside its parent; every reference must stay inside its data file, and inline payloads are at most
8 bytes. Object records are capped at 4096 arguments, transforms at 64 parts, names at 1024 UTF-16
units, colour blocks at 32 tags, paths at 1 048 576 points each and 8 388 608 per document.
Documents are capped at 10 000 pages, 10 000 layers per page, 100 000 PowerClip contents groups,
10 000 guidelines per page, 128 levels of groups and PowerClips (deeper content is reported and
skipped) and 500 000 objects visited, PowerClip contents and repeated master layers included.
PowerClip cycles are cut. An object whose placement is not finite or lands more than 10⁹ pt from
the page is skipped with a warning. Paint tables are capped at 100 000 fill and 100 000 outline
records, 64 items per record, 256 stops per fountain, 4096 mesh nodes and outline widths of
10⁹ world units; placement coefficients must be finite, at most 10⁹ and non-singular. The style
table must have exactly the known record sizes and at most 100 000 records per section. Text
stories are capped at 4096 styles, 1 Mi characters and 64-unit language tags; JSON styles at 1 MiB,
32 levels and 100 000 values; font tables at 65 536 records, 256-unit names and 4 MiB; the
document palette at 1 MiB and 65 536 entries; the text index at 16 MiB. Pictures are at most
65 535 pixels a side and 64 Mi pixels each, every block and palette must lie inside its record,
row strides must hold a row, buffers are reserved fallibly, and every decoded pixel is charged to
the shared budget (a picture beyond it is reported and its outline kept). Previews are at most
16 MiB and 4096 pixels a side, with every PNG chunk checksum verified. Malformed input returns an
`Error`; the crate has no panics outside tests and runs unchanged on `wasm32-unknown-unknown`.

## Privacy

CorelDRAW documents carry XMP metadata (author, dates, identifiers, fonts used), a plain-text
index of every text and name in the document, and colour profiles. This reader never imports
`META-INF/metadata.xml` or the embedded colour profiles (CMYK values come through unconverted).
It reads `color/docPalette.xml` for swatch names, and `META-INF/textinfo.xml` only to recover
the characters of a text whose story is stored in a layout it cannot decode directly.
