//! Bounded output reads and slicer estimate comments.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use base64::Engine as _;

use super::{OutputKind, Prepared};
use crate::backend::Estimate;

const REGION: u64 = 256 << 10;
const MAX_GCODE: u64 = 512 << 20;
const MAX_PNG: u64 = 4 << 20;
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

type Result<T> = std::result::Result<T, String>;

pub(super) fn finish(
    input: &Path,
    gcode: &Path,
    archive: &Path,
    kind: OutputKind,
    orca: bool,
) -> Result<Prepared> {
    let folder = input.parent().ok_or("Cannot locate job folder.")?;
    let preview = folder.join("preview.png");
    let mut have_preview = false;
    if orca {
        let mut zip = open_zip(archive)?;
        {
            let plate = zip
                .by_name("Metadata/plate_1.gcode")
                .map_err(|_| "Slicer 3MF has no Metadata/plate_1.gcode.")?;
            copy_bounded(plate, gcode, MAX_GCODE)?;
        }
        match zip.by_name("Metadata/plate_1.png") {
            Ok(png) => {
                copy_bounded(png, &preview, MAX_PNG)?;
                have_preview = png_file(&preview)?;
            }
            Err(zip::result::ZipError::FileNotFound) => (),
            Err(e) => return Err(format!("Cannot read plate preview: {e}")),
        };
    }
    let regions = comment_regions(gcode)?;
    let mut estimate = parse(&regions);
    if !have_preview {
        if let Some(png) = thumbnail(&regions) {
            std::fs::write(&preview, png).map_err(|e| format!("Cannot save preview: {e}"))?;
            have_preview = true;
        }
    }
    if have_preview {
        estimate.preview = Some(preview.clone());
    }
    if !orca && kind == OutputKind::ThreeMf {
        let mut source = open_zip(input)?;
        let mut dest = zip::ZipWriter::new(File::create(archive).map_err(io_error)?);
        for index in 0..source.len() {
            let entry = source.by_index(index).map_err(zip_error)?;
            if matches!(
                entry.name().map_err(zip_error)?.as_ref(),
                "Metadata/plate_1.gcode" | "Metadata/plate_1.png"
            ) {
                continue;
            }
            dest.raw_copy_file(entry).map_err(zip_error)?;
        }
        for (name, path) in [
            ("Metadata/plate_1.gcode", Some(gcode)),
            ("Metadata/plate_1.png", estimate.preview.as_deref()),
        ] {
            if let Some(path) = path {
                dest.start_file(name, zip::write::SimpleFileOptions::default())
                    .map_err(zip_error)?;
                std::io::copy(&mut File::open(path).map_err(io_error)?, &mut dest)
                    .map_err(io_error)?;
            }
        }
        dest.finish().map_err(zip_error)?;
    }
    Ok(Prepared {
        path: if kind == OutputKind::Gcode {
            gcode.into()
        } else {
            archive.into()
        },
        estimate,
    })
}

fn io_error(error: std::io::Error) -> String {
    format!("Cannot read or save slicer output: {error}")
}
fn zip_error(error: zip::result::ZipError) -> String {
    format!("Cannot read or save slicer 3MF: {error}")
}
fn open_zip(path: &Path) -> Result<zip::ZipArchive<File>> {
    let file = File::open(path).map_err(io_error)?;
    if file.metadata().map_err(io_error)?.len() > MAX_GCODE {
        return Err("Slicer 3MF exceeds 512 MiB.".into());
    }
    let zip = zip::ZipArchive::new(file).map_err(zip_error)?;
    if zip.len() > 5000 {
        return Err("Slicer 3MF exceeds 5,000 entries.".into());
    }
    Ok(zip)
}
fn copy_bounded(reader: impl Read, path: &Path, limit: u64) -> Result<()> {
    let mut file = File::create(path).map_err(io_error)?;
    let bytes = std::io::copy(&mut reader.take(limit + 1), &mut file).map_err(io_error)?;
    if bytes > limit {
        return Err(format!("Slicer output exceeds {limit} bytes."));
    }
    file.flush().map_err(io_error)
}
fn png_file(path: &Path) -> Result<bool> {
    let mut bytes = [0; 8];
    Ok(File::open(path)
        .map_err(io_error)?
        .read_exact(&mut bytes)
        .is_ok()
        && bytes == PNG)
}
fn comment_regions(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(io_error)?;
    let size = file.metadata().map_err(io_error)?.len();
    if size > MAX_GCODE {
        return Err("Slicer G-code exceeds 512 MiB.".into());
    }
    let mut head = Vec::new();
    (&mut file)
        .take(REGION)
        .read_to_end(&mut head)
        .map_err(io_error)?;
    if head.starts_with(b"GCDE") {
        return Err("Slicer wrote binary G-code. Turn binary G-code off.".into());
    }
    let mut text = String::from_utf8_lossy(&head).into_owned();
    if size > REGION {
        // Drop partial boundary lines so truncated values cannot become estimates.
        if let Some(last) = text.rfind('\n') {
            text.truncate(last + 1);
        } else {
            text.clear();
        }
        let start = size.saturating_sub(REGION).max(REGION);
        file.seek(SeekFrom::Start(start.saturating_sub(1)))
            .map_err(io_error)?;
        let mut tail = Vec::new();
        file.take(REGION + 1)
            .read_to_end(&mut tail)
            .map_err(io_error)?;
        let tail = String::from_utf8_lossy(&tail);
        if let Some((_, lines)) = tail.split_once('\n') {
            text.push_str(lines);
        }
    }
    Ok(text)
}

fn parse(text: &str) -> Estimate {
    let mut estimate = Estimate::default();
    for line in text
        .lines()
        .filter_map(|s| s.trim().strip_prefix(';'))
        .flat_map(|s| s.split(';'))
    {
        let Some((key, value)) = line.split_once('=').or_else(|| line.split_once(':')) else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "estimated printing time (normal mode)"
            | "model printing time"
            | "total estimated time" => {
                // Orca's model time excludes preparation; prefer the total when present.
                if key != "model printing time" || estimate.seconds.is_none() {
                    estimate.seconds = time(value);
                }
            }
            "filament used [mm]" | "total filament length [mm]" => {
                estimate.filament_mm = numbers(value)
            }
            "filament used [g]" | "total filament weight [g]" => {
                estimate.filament_g = numbers(value)
            }
            "total layers count" | "total layer number" | "layer_count" => {
                estimate.layers = value.parse().ok()
            }
            _ => (),
        }
    }
    estimate
}
fn numbers(text: &str) -> Option<f64> {
    let mut total = 0.0;
    for value in text.split(',') {
        let number: f64 = value.trim().parse().ok()?;
        if !number.is_finite() || number < 0.0 {
            return None;
        }
        total += number;
    }
    total.is_finite().then_some(total)
}
fn time(text: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut digits = String::new();
    let mut any = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if c.is_whitespace() {
            continue;
        } else {
            let factor = match c {
                'd' => 86400,
                'h' => 3600,
                'm' => 60,
                's' => 1,
                _ => return None,
            };
            total = total.checked_add(digits.parse::<u64>().ok()?.checked_mul(factor)?)?;
            digits.clear();
            any = true;
        }
    }
    (any && digits.is_empty()).then_some(total)
}
fn thumbnail(text: &str) -> Option<Vec<u8>> {
    let mut encoded = String::new();
    let mut active = false;
    for line in text
        .lines()
        .filter_map(|s| s.trim().strip_prefix(';'))
        .map(str::trim)
    {
        if line.starts_with("thumbnail begin ") {
            encoded.clear();
            active = true;
        } else if line == "thumbnail end" && active {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&encoded)
                .ok()?;
            if bytes.starts_with(PNG) {
                return Some(bytes);
            }
            active = false;
        } else if active {
            encoded.push_str(line);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn comments() {
        let prusa = parse(
            "; estimated printing time (normal mode) = 1d 2h 3m 4s\n; filament used [mm] = 120, 30\n; filament used [g] = 2.5\n; total layers count = 20",
        );
        assert_eq!(prusa.seconds, Some(93784));
        assert_eq!(prusa.filament_mm, Some(150.0));
        assert_eq!(prusa.filament_g, Some(2.5));
        assert_eq!(prusa.layers, Some(20));
        let orca = parse(
            "; model printing time: 1h 2m 3s; total estimated time: 1h 3m 4s\n; total filament length [mm] : 321.5\n; total filament weight [g] : 4.2\n; total layer number: 42",
        );
        assert_eq!(orca.seconds, Some(3784));
        assert_eq!(orca.filament_mm, Some(321.5));
        assert_eq!(orca.filament_g, Some(4.2));
        assert_eq!(orca.layers, Some(42));
        let absent = parse("G1 X0\n; filament used [g] = NaN\n; layer_count = -1\n");
        assert!(
            absent.seconds.is_none()
                && absent.filament_mm.is_none()
                && absent.filament_g.is_none()
                && absent.layers.is_none()
        );
    }
    #[test]
    fn reads_only_head_and_tail_and_bounds_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.gcode");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "; estimated printing time (normal mode) = 4m").unwrap();
        file.write_all(&vec![b'\n'; REGION as usize]).unwrap();
        writeln!(file, "; filament used [g] = 999").unwrap();
        file.write_all(&vec![b'\n'; REGION as usize]).unwrap();
        writeln!(file, "; filament used [mm] = 100").unwrap();
        drop(file);
        let estimate = parse(&comment_regions(&path).unwrap());
        assert_eq!(estimate.seconds, Some(240));
        assert_eq!(estimate.filament_mm, Some(100.0));
        assert!(estimate.filament_g.is_none());
        assert!(copy_bounded(std::io::Cursor::new(b"12345"), &path, 4).is_err());
        std::fs::write(&path, b"GCDEbinary").unwrap();
        assert!(comment_regions(&path).unwrap_err().contains("binary"));
        File::create(&path).unwrap().set_len(MAX_GCODE + 1).unwrap();
        assert!(comment_regions(&path).unwrap_err().contains("512 MiB"));
    }
}
