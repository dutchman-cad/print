//! Bounded 3MF checks before job preparation.

use dutchman_serve::upload::{unzip, ZipLimits};
use std::fs::{self, File};
use std::path::Path;

/// Checks a 3MF using the same path, entry, and expanded byte limits as project uploads.
/// `scratch` must be a new folder in the private job directory.
pub fn check_3mf(path: &Path, scratch: &Path, limits: ZipLimits) -> Result<(), String> {
    let file = File::open(path).map_err(|_| "Cannot read the 3MF file.")?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| "Choose a 3MF zip file.")?;
    if zip.len() > limits.entries {
        return Err("The 3MF holds too many zip entries.".into());
    }
    let mut model = false;
    for index in 0..zip.len() {
        let entry = zip
            .by_index(index)
            .map_err(|_| "Cannot read the 3MF zip entries.")?;
        if let Some(name) = entry.enclosed_name() {
            model |= !entry.is_symlink()
                && !entry.is_dir()
                && name.parent() == Some(Path::new("3D"))
                && name.extension().is_some_and(|ext| ext == "model");
        }
    }
    if !model {
        return Err("Choose a 3MF zip with a 3D/*.model part.".into());
    }
    drop(zip);
    fs::create_dir(scratch).map_err(|_| "Cannot create the 3MF check folder.")?;
    let result = (|| {
        let archive = scratch.join("check.zip");
        fs::copy(path, &archive).map_err(|_| "Cannot read the 3MF file.")?;
        unzip(&archive, scratch, &mut { limits })?;
        Ok(())
    })();
    let _ = fs::remove_dir_all(scratch);
    result
}
