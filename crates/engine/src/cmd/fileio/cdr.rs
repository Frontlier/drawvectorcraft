//! CorelDRAW (`.cdr`) import, in three steps; the original file is never written.
//!
//! 1. The native reader (`vectorcraft-cdr`, pure Rust, the web build too) reads the document and
//!    [`native`] maps it to editable art: pages, layers, curves, fills, outlines, gradients,
//!    transparency, text, bitmaps and the user's own editable PowerClips.
//! 2. When it can't (layouts it refuses, such as the `content/riffData.cdr` ZIPs of the X4–X8 era
//!    and the bare RIFF files before them, or a damaged file), the desktop app runs the local,
//!    pinned libcdr converter in a child process and imports the bounded SVG it returns. PowerClip
//!    markers of VectorCraft's patched converter are restored as PowerClips ([`powerclip`]);
//!    output without them is accepted with a warning.
//! 3. When the converter is missing or fails too, the embedded preview picture opens alone
//!    (`preview_only`), with one warning saying why; Place, templates and libraries refuse it.
//!
//! Converter backend: https://github.com/WaltRiceJr/inkscape-extension-cdr/tree/v0.2.1
//! Public format reference: https://github.com/photopea/CDR-specification

use std::io::Cursor;

use vectorcraft_doc::Document;

mod native;
mod powerclip;

use super::load::{err, raster_doc};
use crate::Result;

/// What a CorelDRAW file opened as.
pub(super) struct Imported {
    pub doc: Document,
    pub warnings: Vec<String>,
    /// Only the embedded preview: never placed, used as a template or mined for styles.
    pub preview_only: bool,
}

/// The converter step: the file's bytes → SVG text, or why not.
pub(super) type Converter<'a> = &'a dyn Fn(&[u8]) -> std::result::Result<String, String>;

/// The first warning of a document the native reader read.
pub(super) const NATIVE_NOTE: &str = "CDR read natively as editable objects. Fonts, colour profiles and approximated effects may differ. Save a new .vectorcraft, SVG or PDF copy; writing CDR is not supported.";
/// The first warning of a document the converter read.
pub(super) const CONVERTED_NOTE: &str = "CDR imported as editable objects. Fonts, colour profiles and unsupported effects may differ. Save a new .vectorcraft, SVG or PDF copy; writing CDR is not supported.";
/// Converter output without VectorCraft's PowerClip markers (a stock converter).
pub(super) const UNMARKED_POWERCLIPS: &str =
    "PowerClips, if any, came in as plain clipping groups or flattened art: this CDR converter does not mark them.";
/// Why the converter didn't read a file when the caller turned it off ([`super::LoadOptions::cdr_converter`]).
const CONVERTER_OFF: &str = "the CDR converter is not used for this import";

/// RIFF CDR, or a modern ZIP with a drawing stream in its central directory. Never classify an
/// arbitrary ZIP as CDR. The converter validates the actual container before reading its art.
pub(super) fn sniff(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"RIFF") && bytes.get(8..11).is_some_and(|s| s.eq_ignore_ascii_case(b"CDR")) {
        return true;
    }
    if !bytes.starts_with(b"PK\x03\x04") {
        return false;
    }
    let tail = bytes.get(bytes.len().saturating_sub(65_557)..).unwrap_or_default();
    [b"content/root.dat".as_slice(), b"content/riffData.cdr", b"content/riffdata.cdr"].iter().any(|name| tail.windows(name.len()).any(|s| s == *name))
}

/// Open a CorelDRAW file (`name`: its file name, for the preview's title): natively, else through
/// the local converter (unless `converter` is false), else as its embedded preview.
pub(super) fn import(name: &str, bytes: &[u8], converter: bool) -> Result<Imported> {
    let run: Converter<'_> = &convert;
    import_with(name, bytes, converter.then_some(run))
}

/// [`import`] with the converter step given (`None`: not used), so each step can be tested alone.
pub(super) fn import_with(name: &str, bytes: &[u8], converter: Option<Converter<'_>>) -> Result<Imported> {
    let native = match native::import(bytes) {
        Ok((doc, mut warnings)) => {
            warnings.insert(0, NATIVE_NOTE.into());
            return Ok(Imported { doc, warnings, preview_only: false });
        }
        Err(why) => why,
    };
    let converter = match converter.ok_or_else(|| CONVERTER_OFF.to_string()).and_then(|c| converted(bytes, c)) {
        Ok((doc, mut warnings)) => {
            warnings.splice(
                0..0,
                [
                    CONVERTED_NOTE.to_string(),
                    format!("The native CDR reader could not read this file ({native}); the local CDR converter opened it instead."),
                ],
            );
            return Ok(Imported { doc, warnings, preview_only: false });
        }
        Err(why) => why,
    };
    let (doc, warning) = preview(name, bytes, &native, &converter)?;
    Ok(Imported { doc, warnings: vec![warning], preview_only: true })
}

/// The converter step: check the container, convert, import the SVG and restore the PowerClips
/// it marks.
fn converted(bytes: &[u8], convert: Converter<'_>) -> std::result::Result<(Document, Vec<String>), String> {
    if bytes.is_empty() || bytes.len() > 128 << 20 || !sniff(bytes) {
        return Err("Invalid or unsupported CDR container (maximum input: 128 MiB)".into());
    }
    if bytes.starts_with(b"RIFF") {
        let size = bytes.get(4..8).and_then(|s| <[u8; 4]>::try_from(s).ok()).map(u32::from_le_bytes).unwrap_or(0);
        // RIFF's size includes the four-byte form type. A drawing needs at least one record
        // header after it. Refuse empty/truncated roots before a native parser can loop on them.
        if size < 12 || u64::from(size) + 8 > bytes.len() as u64 {
            return Err("CDR RIFF container is truncated or contains no drawing records".into());
        }
    }
    let svg = convert(bytes)?;
    // No source folder or file reader: a converter result cannot read links from the host.
    let (mut doc, mut warnings) = vectorcraft_svg::import_with_report(&svg).map_err(|e| e.to_string())?;
    if svg.contains("data-vectorcraft-powerclip=\"1\"") {
        powerclip::restore(&mut doc)?;
    } else {
        warnings.insert(0, UNMARKED_POWERCLIPS.into());
    }
    Ok((doc, warnings))
}

/// The embedded preview, for files neither the native reader nor the converter could read; an
/// error naming all three reasons, the native reader's first, when it can't be shown either.
fn preview(name: &str, bytes: &[u8], native: &str, converter: &str) -> Result<(Document, String)> {
    let failed = |e: &dyn std::fmt::Display| {
        err(format!("{native}; the CDR converter could not read it either ({converter}), and its embedded preview can't be shown ({e})"))
    };
    let p = vectorcraft_cdr::preview(bytes).map_err(|e| failed(&e))?;
    // `raster_doc` normally probes PNG dimensions and keeps its encoded bytes. Verify this
    // untrusted preview's complete pixels first, so corrupt data cannot create a blank image.
    let mut reader = image::ImageReader::with_format(Cursor::new(&p.png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(vectorcraft_cdr::MAX_PREVIEW_DIMENSION);
    limits.max_image_height = Some(vectorcraft_cdr::MAX_PREVIEW_DIMENSION);
    limits.max_alloc = Some(128 << 20);
    reader.limits(limits);
    let _pixels = reader.decode().map_err(|e| failed(&e))?;
    let d = raster_doc(name, &p.png).map_err(|e| failed(&e))?;
    Ok((
        d,
        format!(
            "Neither the native CDR reader ({native}) nor the CDR converter ({converter}) could read this document. Opened only its embedded {}×{} PNG preview: no layers, vectors, text or pages. Save a new copy; the CorelDRAW source cannot be saved back.",
            p.width, p.height
        ),
    ))
}

#[cfg(target_arch = "wasm32")]
fn convert(_: &[u8]) -> std::result::Result<String, String> {
    Err("CDR import requires the desktop application and its local CDR converter".into())
}

#[cfg(not(target_arch = "wasm32"))]
fn convert(bytes: &[u8]) -> std::result::Result<String, String> {
    converter::convert(bytes)
}

/// The local converter process (desktop only).
#[cfg(not(target_arch = "wasm32"))]
mod converter {
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const MAX_OUTPUT: u64 = 256 << 20;
    const MAX_DIAGNOSTIC: u64 = 64 << 10;
    const TIMEOUT: Duration = Duration::from_secs(55);

    fn io(e: impl std::fmt::Display) -> String {
        format!("CDR import: {e}")
    }

    fn converter() -> std::result::Result<PathBuf, String> {
        if let Some(path) = std::env::var_os("VECTORCRAFT_CDR_CONVERTER") {
            let path = PathBuf::from(path);
            if path.is_absolute() && path.is_file() {
                return Ok(path);
            }
            return Err("VECTORCRAFT_CDR_CONVERTER must name an existing absolute cdr2svg executable".into());
        }
        let exe = std::env::current_exe().map_err(io)?;
        let name = if cfg!(windows) { "cdr2svg.exe" } else { "cdr2svg" };
        // Unit-test executables are in deps/, next to the app's directory.
        for parent in exe.parent().into_iter().chain(exe.parent().and_then(|p| p.parent())) {
            let candidate = parent.join("cdr").join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        Err("CDR converter is not installed. On Windows run packaging/windows/Install-CdrRuntime.ps1 for this build; on other desktop systems set VECTORCRAFT_CDR_CONVERTER to an absolute cdr2svg executable.".into())
    }

    /// Kill and reap on every early return, including a failed size check or read.
    struct Running(Child);
    impl Drop for Running {
        fn drop(&mut self) {
            // A finished child may already have been reaped by try_wait; these are best-effort.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    pub(super) fn convert(bytes: &[u8]) -> std::result::Result<String, String> {
        let executable = converter()?;
        let dir = tempfile::Builder::new().prefix("vectorcraft-cdr-").tempdir().map_err(io)?;
        let input = dir.path().join("input.cdr");
        let output = dir.path().join("output.svg");
        let diagnostics = dir.path().join("diagnostics.txt");
        let mut source = std::fs::File::create(&input).map_err(io)?;
        source.write_all(bytes).map_err(io)?;
        drop(source);
        let stdout = std::fs::File::create(&output).map_err(io)?;
        let stderr = std::fs::File::create(&diagnostics).map_err(io)?;
        let mut command = Command::new(executable);
        command.arg("--no-clip").arg(&input).current_dir(dir.path()).stdin(Stdio::null()).stdout(stdout).stderr(stderr);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: File > Open must not flash a console.
        }
        let mut child = Running(command.spawn().map_err(io)?);
        let started = Instant::now();
        let status = loop {
            if std::fs::metadata(&output).map_err(io)?.len() > MAX_OUTPUT || std::fs::metadata(&diagnostics).map_err(io)?.len() > MAX_DIAGNOSTIC {
                return Err("CDR converter exceeded its output limit".into());
            }
            if let Some(status) = child.0.try_wait().map_err(io)? {
                break status;
            }
            if started.elapsed() >= TIMEOUT {
                return Err("CDR converter timed out; the document was not opened".into());
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        // Final sizes matter too: the process can exit between the previous check and try_wait.
        if std::fs::metadata(&output).map_err(io)?.len() > MAX_OUTPUT || std::fs::metadata(&diagnostics).map_err(io)?.len() > MAX_DIAGNOSTIC {
            return Err("CDR converter exceeded its output limit".into());
        }
        if !status.success() {
            let detail = std::fs::read_to_string(&diagnostics).unwrap_or_default();
            return Err(format!("CDR converter could not read the document ({status}): {}", detail.trim()));
        }
        let mut svg = String::new();
        std::fs::File::open(&output).map_err(io)?.take(MAX_OUTPUT + 1).read_to_string(&mut svg).map_err(io)?;
        if svg.is_empty() || svg.len() as u64 > MAX_OUTPUT {
            return Err("CDR converter returned empty or oversized SVG".into());
        }
        Ok(svg)
    }
}
