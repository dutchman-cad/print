//! External slicer commands. Presets belong to the installed slicer.

mod output;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::backend::{Estimate, Options};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub kind: String,
    pub path: Option<PathBuf>,
    pub datadir: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    600
}

impl Config {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !matches!(self.kind.as_str(), "prusaslicer" | "orcaslicer") {
            return Err(format!(
                "Slicer kind {:?} is not supported yet. Use prusaslicer or orcaslicer.",
                self.kind
            ));
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > 86400 {
            return Err("Set slicer.timeout_seconds between 1 and 86400.".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    Gcode,
    ThreeMf,
}

pub struct Prepared {
    pub path: PathBuf,
    pub estimate: Estimate,
}

pub struct Slicer {
    config: Config,
    executable: PathBuf,
}

impl Slicer {
    pub fn new(config: Config) -> Result<Self, String> {
        config.validate()?;
        let executable = executable(&config)?;
        Ok(Self { config, executable })
    }

    /// Runs in the input's private job folder. The caller chooses the printer's file kind.
    pub async fn prepare(
        &self,
        input: &Path,
        printer: &crate::config::Printer,
        options: &Options,
        kind: OutputKind,
    ) -> Result<Prepared, String> {
        options.validate()?;
        for name in [
            &printer.printer_profile,
            &printer.print_profile,
            &printer.filament_profile,
        ]
        .into_iter()
        .flatten()
        {
            crate::backend::validate_profile_name(name)?;
        }
        let input =
            std::fs::canonicalize(input).map_err(|e| format!("Cannot locate the 3MF: {e}"))?;
        let folder = input.parent().ok_or("Choose a 3MF in a job folder.")?;
        let gcode = folder.join("sliced.gcode");
        let archive = folder.join("sliced.3mf");
        if input.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.eq_ignore_ascii_case("sliced.3mf") || name.eq_ignore_ascii_case("sliced.gcode")
        }) {
            return Err("Choose an input filename other than sliced.3mf or sliced.gcode.".into());
        }
        // A successful command must create fresh output, including on retries.
        for path in [&gcode, &archive, &folder.join("preview.png")] {
            match std::fs::remove_file(path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(format!("Cannot clear slicer output: {e}")),
            }
        }
        let mut command = tokio::process::Command::new(&self.executable);
        command
            .current_dir(folder)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(datadir) = &self.config.datadir {
            command.arg(flag_value("--datadir", absolute(datadir)?));
        }
        let profiles = [
            options
                .printer_profile
                .as_ref()
                .or(printer.printer_profile.as_ref()),
            options
                .print_profile
                .as_ref()
                .or(printer.print_profile.as_ref()),
            options
                .filament_profile
                .as_ref()
                .or(printer.filament_profile.as_ref()),
        ];
        if self.config.kind == "prusaslicer" {
            command
                .args(["--export-gcode", "--no-binary-gcode"])
                .arg(flag_value("--output", &gcode));
            for (flag, name) in ["--printer-profile", "--print-profile", "--material-profile"]
                .into_iter()
                .zip(profiles)
            {
                if let Some(name) = name {
                    command.arg(format!("{flag}={name}"));
                }
            }
        } else {
            let root = self
                .config
                .datadir
                .clone()
                .or_else(|| orca_data_folder(&self.executable))
                .ok_or("Cannot find OrcaSlicer data. Set slicer.datadir.")?;
            let root = absolute(&root)?;
            let mut settings = Vec::new();
            for (category, name) in ["machine", "process", "filament"].into_iter().zip(profiles) {
                if let Some(name) = name {
                    let lookup_root = root.clone();
                    let lookup_name = name.clone();
                    let lookup_folder = folder.to_path_buf();
                    let path = tokio::task::spawn_blocking(move || {
                        preset(&lookup_root, category, &lookup_name, &lookup_folder)
                    })
                    .await
                    .map_err(|_| "Cannot search Orca presets.".to_string())??;
                    if path.to_string_lossy().contains(';') {
                        return Err(
                            "Orca preset paths cannot contain semicolons. Move slicer.datadir."
                                .into(),
                        );
                    }
                    if category == "filament" {
                        command.arg(flag_value("--load-filaments", path));
                    } else {
                        settings.push(path);
                    }
                }
            }
            if !settings.is_empty() {
                let mut joined = OsString::new();
                for path in settings {
                    if !joined.is_empty() {
                        joined.push(";");
                    }
                    joined.push(path);
                }
                command.arg(flag_value("--load-settings", joined));
            }
            command
                .args(["--slice=0", "--export-3mf=sliced.3mf"])
                .arg(flag_value("--outputdir", folder));
        }
        command.arg(&input);
        let mut child = command
            .spawn()
            .map_err(|e| format!("Cannot run slicer: {e}. Set slicer.path to its executable."))?;
        let stdout = child.stdout.take().ok_or("Cannot capture slicer stdout.")?;
        let stderr = child.stderr.take().ok_or("Cannot capture slicer stderr.")?;
        // Drain both pipes even after the retained tail is full.
        let mut out = Vec::new();
        let mut err = Vec::new();
        let run = async {
            let (status, stdout, stderr) =
                tokio::join!(child.wait(), tail(stdout, &mut out), tail(stderr, &mut err));
            stdout?;
            stderr?;
            status.map_err(|e| format!("Cannot wait for slicer: {e}"))
        };
        let status = match tokio::time::timeout(
            Duration::from_secs(self.config.timeout_seconds),
            run,
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                let _ = child.kill().await;
                return Err(format!(
                    "Slicer timed out after {} seconds. Check profiles or set slicer.timeout_seconds.\nstdout:\n{}\nstderr:\n{}",
                    self.config.timeout_seconds,
                    last_lines(&out),
                    last_lines(&err)
                ));
            }
        };
        if !status.success() {
            return Err(format!(
                "Slicer failed ({status}). Check its profiles.\nstdout:\n{}\nstderr:\n{}",
                last_lines(&out),
                last_lines(&err)
            ));
        }
        let orca = self.config.kind == "orcaslicer";
        tokio::task::spawn_blocking(move || output::finish(&input, &gcode, &archive, kind, orca))
            .await
            .map_err(|_| "Cannot read slicer output.".to_string())?
    }
}

async fn tail(mut reader: impl AsyncRead + Unpin, result: &mut Vec<u8>) -> Result<(), String> {
    let mut buffer = [0; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| "Cannot read slicer diagnostics.")?;
        if count == 0 {
            return Ok(());
        }
        result.extend_from_slice(
            buffer
                .get(..count)
                .ok_or("Cannot read slicer diagnostics.")?,
        );
        let excess = result.len().saturating_sub(8192);
        result.drain(..excess);
    }
}
fn last_lines(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<_> = text.lines().rev().take(20).collect();
    lines.reverse();
    lines
        .join("\n")
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}
fn absolute(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(path))
            .map_err(|_| "Cannot locate the working folder.".into())
    }
}

fn executable(config: &Config) -> Result<PathBuf, String> {
    executable_candidates(
        config,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )?
    .into_iter()
    .find(|path| is_executable(path))
    .ok_or_else(|| {
        format!(
            "Cannot find {}. Install it separately and set slicer.path to its executable.",
            config.kind
        )
    })
    .and_then(|path| absolute(&path))
}

fn executable_candidates(
    config: &Config,
    paths: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Result<Vec<PathBuf>, String> {
    let _ = home;
    let names: &[&str] = if config.kind == "prusaslicer" {
        &["prusa-slicer-console", "prusa-slicer", "PrusaSlicer"]
    } else {
        &["orca-slicer", "OrcaSlicer"]
    };
    let mut candidates = Vec::new();
    if let Some(path) = &config.path {
        candidates.push(absolute(path)?);
    } else {
        if let Some(paths) = paths {
            for dir in std::env::split_paths(paths) {
                for name in names {
                    candidates.push(dir.join(if cfg!(windows) {
                        format!("{name}.exe")
                    } else {
                        (*name).into()
                    }));
                }
            }
        }
        let app = if config.kind == "prusaslicer" {
            "PrusaSlicer"
        } else {
            "OrcaSlicer"
        };
        #[cfg(target_os = "macos")]
        for base in [
            PathBuf::from("/Applications"),
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join("Applications"),
        ] {
            candidates.push(base.join(format!("{app}.app/Contents/MacOS/{app}")));
            candidates.push(base.join(format!(
                "Original Prusa Drivers/{app}.app/Contents/MacOS/{app}"
            )));
        }
        #[cfg(windows)]
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .into_iter()
            .filter_map(std::env::var_os)
        {
            for name in names {
                candidates.push(PathBuf::from(&base).join(app).join(format!("{name}.exe")));
                candidates.push(
                    PathBuf::from(&base)
                        .join("Prusa3D")
                        .join(app)
                        .join(format!("{name}.exe")),
                );
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        for base in ["/usr/bin", "/usr/local/bin", "/opt"] {
            for name in names {
                candidates.push(PathBuf::from(base).join(name));
                candidates.push(PathBuf::from(base).join(app).join(name));
            }
        }
    }
    #[cfg(target_os = "linux")]
    if config.path.is_none() {
        candidates.extend(flatpak_candidates(&config.kind, home));
    }
    Ok(candidates)
}
#[cfg(target_os = "linux")]
const ORCA_FLATPAK_IDS: &[&str] = &[
    "io.github.softfever.OrcaSlicer",
    "com.orcaslicer.OrcaSlicer",
];

#[cfg(target_os = "linux")]
fn flatpak_candidates(kind: &str, home: Option<&Path>) -> Vec<PathBuf> {
    let ids = if kind == "prusaslicer" {
        &["com.prusa3d.PrusaSlicer"][..]
    } else {
        ORCA_FLATPAK_IDS
    };
    let mut roots = vec![PathBuf::from("/var/lib/flatpak/exports/bin")];
    if let Some(home) = home {
        roots.push(home.join(".local/share/flatpak/exports/bin"));
    }
    roots
        .into_iter()
        .flat_map(|root| ids.iter().map(move |id| root.join(id)))
        .collect()
}

fn orca_data_folder(executable: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(id) = executable.file_name().and_then(|n| n.to_str()) {
        if ORCA_FLATPAK_IDS.contains(&id) {
            return std::env::var_os("HOME").map(|home| {
                PathBuf::from(home)
                    .join(".var/app")
                    .join(id)
                    .join("config/OrcaSlicer")
            });
        }
    }
    let _ = executable;
    data_folder("OrcaSlicer")
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}
fn data_folder(app: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
    base.map(|p| p.join(app))
}

fn preset(root: &Path, category: &str, name: &str, folder: &Path) -> Result<PathBuf, String> {
    let roots = [root.join("user"), root.join("system")];
    let searched = format!(
        "{} and {} (category {category})",
        roots
            .first()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        roots
            .last()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    let mut stack: Vec<_> = roots.into_iter().map(|p| (p, 0)).collect();
    let mut count = 0;
    let mut catalog = Vec::new();
    let mut total_bytes = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(format!(
                    "Cannot search Orca presets in {}: {e}",
                    dir.display()
                ));
            }
        };
        for entry in entries {
            count += 1;
            if count > 20000 {
                return Err(format!("Too many Orca preset files. Searched {searched}."));
            }
            let entry = entry.map_err(|e| format!("Cannot list Orca presets: {e}"))?;
            let ty = entry
                .file_type()
                .map_err(|e| format!("Cannot check Orca preset: {e}"))?;
            let path = entry.path();
            if ty.is_dir() && depth < 8 {
                stack.push((path, depth + 1));
                continue;
            }
            if !ty.is_file()
                || path
                    .extension()
                    .is_none_or(|e| !e.eq_ignore_ascii_case("json"))
                || !path
                    .ancestors()
                    .skip(1)
                    .take(depth + 1)
                    .any(|p| p.file_name().is_some_and(|n| n == category))
            {
                continue;
            }
            let value = read_preset(&path, &mut total_bytes)?;
            if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
                catalog.push((path, name.to_string()));
            }
        }
    }
    let missing = |name: &str| {
        format!(
            "Orca profile {name:?} was not found. Searched {searched}. Set slicer.datadir to the slicer's data folder."
        )
    };
    let choose = |name: &str, vendor: Option<&Path>| -> Result<usize, String> {
        let matches: Vec<_> = catalog
            .iter()
            .enumerate()
            .filter(|(_, (_, candidate))| candidate == name)
            .collect();
        let local: Vec<_> = matches
            .iter()
            .filter(|(_, (path, _))| vendor.is_some_and(|v| path.starts_with(v)))
            .collect();
        let indices: Vec<_> = if local.is_empty() {
            matches.iter().map(|(i, _)| *i).collect()
        } else {
            local.iter().map(|(i, _)| *i).collect()
        };
        if indices.len() > 1 {
            return Err(format!(
                "Orca profile {name:?} is ambiguous. Searched {searched}. Choose a unique preset name."
            ));
        }
        indices.first().copied().ok_or_else(|| missing(name))
    };
    let mut index = choose(name, None)?;
    let mut chain = Vec::new();
    loop {
        if chain.len() >= 32 || chain.contains(&index) {
            return Err(format!(
                "Orca profile {name:?} has cyclic inheritance or exceeds 32 presets. Save a preset with resolved settings."
            ));
        }
        chain.push(index);
        let (path, _) = catalog.get(index).ok_or("Cannot locate Orca preset.")?;
        let value = read_preset(path, &mut total_bytes)?;
        let parent = match value.get("inherits") {
            None | Some(serde_json::Value::Null) => "",
            Some(serde_json::Value::String(parent)) => parent,
            _ => return Err("Orca preset inheritance is not supported yet. Save a preset with resolved settings.".into()),
        };
        if parent.is_empty() {
            break;
        }
        let vendor = path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == category))
            .and_then(Path::parent);
        index = choose(parent, vendor)?;
    }
    if chain.len() == 1 {
        return catalog
            .get(index)
            .map(|(path, _)| path.clone())
            .ok_or_else(|| missing(name));
    }
    let mut resolved = serde_json::Map::new();
    for index in chain.into_iter().rev() {
        let (path, _) = catalog.get(index).ok_or("Cannot locate Orca preset.")?;
        let value = read_preset(path, &mut total_bytes)?;
        let value = value
            .as_object()
            .ok_or("Cannot read Orca preset settings.")?;
        resolved.extend(value.clone());
        resolved.remove("inherits");
        if serde_json::to_vec(&resolved)
            .map_err(|_| "Cannot encode Orca preset settings.")?
            .len()
            > 1 << 20
        {
            return Err("Resolved Orca preset exceeds 1 MiB.".into());
        }
    }
    let path = folder.join(format!("dutchman-{category}.json"));
    let bytes = serde_json::to_vec(&resolved).map_err(|_| "Cannot encode Orca preset settings.")?;
    if bytes.len() > 1 << 20 {
        return Err("Resolved Orca preset exceeds 1 MiB.".into());
    }
    std::fs::write(&path, bytes).map_err(|e| format!("Cannot save resolved Orca preset: {e}"))?;
    Ok(path)
}

fn read_preset(path: &Path, total_bytes: &mut usize) -> Result<serde_json::Value, String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|e| format!("Cannot open Orca preset: {e}"))?;
    let mut bytes = Vec::new();
    file.take((1 << 20) + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Cannot read Orca preset: {e}"))?;
    if bytes.len() > 1 << 20 {
        return Err("Orca preset exceeds 1 MiB.".into());
    }
    *total_bytes += bytes.len();
    if *total_bytes > 64 << 20 {
        return Err("Orca preset data exceeds 64 MiB. Choose a smaller preset data folder.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| format!("Cannot read Orca preset JSON: {}", path.display()))
}

// Preserve non-UTF-8 paths while keeping each option and its value in one argv entry.
fn flag_value(flag: &str, value: impl AsRef<std::ffi::OsStr>) -> OsString {
    let mut arg = OsString::from(flag);
    arg.push("=");
    arg.push(value);
    arg
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn explicit_then_path_then_native_then_flatpak() {
        let mut config = Config {
            kind: "orcaslicer".into(),
            path: None,
            datadir: None,
            timeout_seconds: 10,
        };
        let home = Path::new("/example-home");
        let paths = std::ffi::OsStr::new("/first:/second");
        let candidates = executable_candidates(&config, Some(paths), Some(home)).unwrap();
        assert_eq!(
            candidates.first().unwrap(),
            &PathBuf::from("/first/orca-slicer")
        );
        let position = |p: &str| candidates.iter().position(|c| c == Path::new(p)).unwrap();
        assert!(position("/first/OrcaSlicer") < position("/second/orca-slicer"));
        assert!(position("/second/OrcaSlicer") < position("/usr/bin/orca-slicer"));
        assert!(
            position("/opt/OrcaSlicer/OrcaSlicer")
                < position("/var/lib/flatpak/exports/bin/io.github.softfever.OrcaSlicer")
        );
        assert!(
            position("/var/lib/flatpak/exports/bin/io.github.softfever.OrcaSlicer")
                < position("/var/lib/flatpak/exports/bin/com.orcaslicer.OrcaSlicer")
        );
        assert!(
            position("/var/lib/flatpak/exports/bin/com.orcaslicer.OrcaSlicer")
                < position(
                    "/example-home/.local/share/flatpak/exports/bin/io.github.softfever.OrcaSlicer"
                )
        );
        config.path = Some(PathBuf::from("/chosen"));
        assert_eq!(
            executable_candidates(&config, Some(paths), Some(home)).unwrap(),
            vec![PathBuf::from("/chosen")]
        );
        assert_eq!(
            flatpak_candidates("prusaslicer", Some(home)),
            vec![
                PathBuf::from("/var/lib/flatpak/exports/bin/com.prusa3d.PrusaSlicer"),
                home.join(".local/share/flatpak/exports/bin/com.prusa3d.PrusaSlicer")
            ]
        );
    }

    #[test]
    fn orca_inheritance_uses_vendor_defaults_and_bounds_cycles() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let vendor = root.join("system/Example/machine");
        let other = root.join("system/Other/machine");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            vendor.join("base.json"),
            r#"{"name":"Base","speed":"50","height":"0.2"}"#,
        )
        .unwrap();
        std::fs::write(other.join("base.json"), r#"{"name":"Base","speed":"999"}"#).unwrap();
        std::fs::write(
            vendor.join("cube.json"),
            r#"{"name":"Cube","inherits":"Base","speed":"75"}"#,
        )
        .unwrap();
        let path = preset(root, "machine", "Cube", root).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value.get("speed").unwrap(), "75");
        assert_eq!(value.get("height").unwrap(), "0.2");
        assert!(value.get("inherits").is_none());
        std::fs::write(
            vendor.join("base.json"),
            r#"{"name":"Base","inherits":"Cube"}"#,
        )
        .unwrap();
        assert!(preset(root, "machine", "Cube", root)
            .unwrap_err()
            .contains("cyclic"));
        std::fs::write(
            vendor.join("base.json"),
            r#"{"name":"Base","inherits":"Missing"}"#,
        )
        .unwrap();
        assert!(preset(root, "machine", "Cube", root)
            .unwrap_err()
            .contains("Missing"));
        for i in 0..33 {
            std::fs::write(vendor.join(format!("chain-{i}.json")), serde_json::json!({"name":format!("Chain {i}"), "inherits":format!("Chain {}", i+1)}).to_string()).unwrap();
        }
        assert!(preset(root, "machine", "Chain 0", root)
            .unwrap_err()
            .contains("32 presets"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn flatpak_data_matches_selected_app_id() {
        for id in ORCA_FLATPAK_IDS {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            assert_eq!(
                orca_data_folder(&PathBuf::from("/var/lib/flatpak/exports/bin").join(id)),
                Some(home.join(".var/app").join(id).join("config/OrcaSlicer"))
            );
        }
        assert_eq!(
            orca_data_folder(Path::new("/usr/bin/orca-slicer")),
            data_folder("OrcaSlicer")
        );
    }
}
