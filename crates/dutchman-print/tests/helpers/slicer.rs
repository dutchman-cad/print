//! Shared synthetic input and exact argv fixture for backend preparation tests.
use std::io::Write as _;
use std::path::{Path, PathBuf};

// Derive expectations after creating the input, preserving platform canonical
// spelling (including macOS /private/var and Windows extended-length prefixes).
pub fn input(dir: &Path, args: impl FnOnce(&Path) -> Vec<String>) -> PathBuf {
    let input = dir.join("input.3mf");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&input).unwrap());
    zip.start_file("3D/test.model", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"<model/>").unwrap();
    zip.finish().unwrap();
    let canonical_input = input.canonicalize().unwrap();
    std::fs::write(
        dir.join("expected.json"),
        serde_json::to_vec(&args(&canonical_input)).unwrap(),
    )
    .unwrap();
    input
}
