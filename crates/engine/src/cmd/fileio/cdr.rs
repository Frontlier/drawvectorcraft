//! CDR import through the local, pinned libcdr converter. The original file is never written.
//! Public format reference: https://github.com/photopea/CDR-specification
//! Backend and source: https://github.com/WaltRiceJr/inkscape-extension-cdr/tree/v0.2.1
//! Native parsing runs in a child process; only bounded SVG returns to the document engine.

use vectorcraft_doc::Document;

use super::load::err;
use crate::Result;

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

pub(super) fn import(bytes: &[u8]) -> Result<(Document, Vec<String>)> {
    if bytes.is_empty() || bytes.len() > 128 << 20 || !sniff(bytes) {
        return Err(err("Invalid or unsupported CDR container (maximum input: 128 MiB)"));
    }
    if bytes.starts_with(b"RIFF") {
        let size = bytes.get(4..8).and_then(|s| <[u8; 4]>::try_from(s).ok()).map(u32::from_le_bytes).unwrap_or(0);
        // RIFF's size includes the four-byte form type. A drawing needs at least one record
        // header after it. Refuse empty/truncated roots before a native parser can loop on them.
        if size < 12 || u64::from(size) + 8 > bytes.len() as u64 {
            return Err(err("CDR RIFF container is truncated or contains no drawing records"));
        }
    }
    let svg = convert(bytes).map_err(err)?;
    // No source folder or file reader: a converter result cannot read links from the host.
    let (doc, mut warnings) = vectorcraft_svg::import_with_report(&svg).map_err(err)?;
    warnings.insert(0, "CDR imported as editable objects. Fonts, colour profiles and unsupported effects may differ. Save a new .vectorcraft, SVG or PDF copy; writing CDR is not supported.".into());
    Ok((doc, warnings))
}

#[cfg(target_arch = "wasm32")]
fn convert(_: &[u8]) -> std::result::Result<String, String> {
    Err("CDR import requires the desktop application and its local CDR converter".into())
}

#[cfg(not(target_arch = "wasm32"))]
fn convert(bytes: &[u8]) -> std::result::Result<String, String> {
    native::convert(bytes)
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
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
