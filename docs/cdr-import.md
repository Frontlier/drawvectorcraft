# CDR import

Last updated: 2026-10-15. Task CDR.1. Desktop import; CDR export is not implemented.

## What works

Open a .cdr with File > Open, drag it onto the desktop app, pass its path on the command line,
or use document.open with a path or dataBase64. File > Place uses the same importer.
Legacy RIFF CDR and newer ZIP CDR containers are detected by their drawing streams, not only the
extension. Paths, text, groups, images and page definitions pass through editable SVG to the
normal document model. Artwork outside pages is retained; the initial desktop view fits the
artwork and pages together. The original page size is not changed.

Save asks for a new .vectorcraft file. SVG and PDF are also available. The .cdr source is never
overwritten by Save and CDR is not advertised as writable. This is import compatibility, not
a lossless CorelDRAW round trip. Fonts must be available locally. Unsupported effects, source
colour profiles, hidden object semantics and text layout may differ. The imported object
structure is the converter's SVG structure, not a claim of identical native CorelDRAW layers.
The browser build retains its existing formats; native CDR parsing requires the desktop helper.

## Backend installation and development

The reader uses the MPL-2.0 libcdr/librevenge cdr2svg backend from
[inkscape-extension-cdr v0.2.1](https://github.com/WaltRiceJr/inkscape-extension-cdr/tree/v0.2.1).
Only the converter and its runtime libraries are used; neither CorelDRAW nor the Inkscape
application needs to be installed. No server, upload or online conversion is involved.

After building the Windows x64 app/CLI:

    powershell -File packaging/windows/Install-CdrRuntime.ps1

For another build directory, pass -TargetDirectory with the cdr subfolder beside the exe.
Keep that cdr directory beside both vectorcraft.exe and vectorcraft-cli.exe when copying a build.
The installer verifies this pinned release archive before extraction:

    SHA-256 955a9b0bdb13e0e10801abd2de9efbccde1a584b6595ceb1df24630c81bebc4a

An absolute VECTORCRAFT_CDR_CONVERTER path can select an equivalent cdr2svg on another desktop OS.
Missing helpers and conversion failures produce an import error and preserve the active document.
The Windows helper is x64. Other architectures need a compatible helper; no universal packaging
or cross-platform runtime QA is claimed by this task.

Each import gets a private temporary directory containing only its input and outputs. The helper
receives argument vectors without a shell, with stdin closed and no console window. The engine
limits input to 128 MiB, SVG to 256 MiB, diagnostics to 64 KiB and conversion time to 55 seconds;
empty or truncated RIFF roots are rejected before the native parser runs. Early returns terminate
and reap the child and temporary files are removed. SVG import cannot resolve external host file
links. This child-process boundary contains a parser crash; it is not an operating-system security
sandbox.

The downloaded native runtime stays in ignored build output. No third-party C++ sources, GPL
application sources, or user artwork are committed. The app remains Rust; the optional desktop
converter is a separate native dependency. Redistribution needs the converter's MPL notice and
corresponding sources, ICU Unicode licence, lcms2 MIT licence, zlib licence, GCC Runtime Library
Exception notices and winpthreads MIT/BSD notices. The local setup script is not a release
licensing or MSI packaging implementation.

## Evidence and verification

The user's local model was inspected with REA 6.4.0 inspect-artifact. It is a ZIP with 23 entries
plus the root node, including content/root.dat, page1.dat, Bitmaps.dat and embedded resources.

- Input SHA-256: 7798c17f49951e6508deb2045122b5812e188304c943ff5029a8ab8a6c6e51e6.
- REA evidence: ev_868f9658f91d8fcd4dfd59a536d946b390c624af197020bfe4d6480aca9adede.
- Underlying artifact graph evidence: ev_892b71a6415d6b4a176168822eda6d148b5c137cc1373f1b90c456410d6cb4c7.
- Observed converter output: one page, 603 path elements, 14 text elements, five image elements.
- Observed editor model: 701 paths (the SVG importer splits some paths), 14 text objects,
  three image objects, 24 compound paths and 194 groups. An imported path was transformed
  and undone; imported text was edited and undone through the running desktop app.
- The saved native copy reopens with the same object-kind counts; placing the source
  retains its pasteboard vectors. A missing helper produces a normal CLI error.
- Initial fit on the local model is about 1.22%. Fits, saved views and pointer zoom now allow
  values below the first 3.13% zoom preset so the large pasteboard is not cropped. Zoom Out
  continues to zoom away from artwork at those scales, and custom zoom values are retained.
- Import status and error messages have translations in all 15 bundled language catalogs,
  including Brazilian Portuguese.
- Fidelity limit observed locally: the converter emits four blue paths with zero fill
  opacity, while the embedded thumbnail shows blue artwork. That cached thumbnail is
  not an independently verified current render of the native drawing; this import is
  not pixel-identical to it. The transparent paths remain in the editable document.
- Scope: inventory is static REA evidence; conversion and editor tests are separate runtime evidence.
  This single local model does not establish compatibility with every CDR version or feature.
- The CDR and generated artwork remain local, outside version control.

Engine regressions: tests_cdr covers format/filter exposure, refused CDR saves, content detection,
malformed data and preservation of an existing document. import_fuzz covers malformed containers.
The ignored local_cdr_opens_as_editable_art_and_saves_without_changing_its_source test uses the
VECTORCRAFT_CDR_TEST_FILE environment variable; it checks editable paths and text, edit/undo,
native serialization/reopen and original-file preservation. The UI regression covers first fit
from Home with artwork outside the original page.

Final Windows x64 verification on 2026-10-15: cargo xtask ci passed all eight gates (fmt, clippy,
workspace tests, pdf-deps, assets, brands, layers and wasm). The local CDR integration test passed
separately. The updated desktop executable also passed imported-vector and text edits with undo,
zoom navigation below 3.13%, save, close and reopen; the source SHA-256 remained unchanged.

Public format reference: https://github.com/photopea/CDR-specification.
