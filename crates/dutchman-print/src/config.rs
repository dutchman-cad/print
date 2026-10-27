//! Printer settings. Secrets are never serializable or included in diagnostics.

use std::collections::{BTreeMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const MAX_CONFIG_BYTES: u64 = 1 << 20;
pub const MAX_PRINTERS: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub backend: Backend,
    pub slicer: Option<crate::slicer::Config>,
    #[serde(default)]
    pub slicers: BTreeMap<String, crate::slicer::Config>,
    #[serde(default)]
    pub printers: Vec<Printer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub kind: String,
    #[serde(default)]
    pub slice: bool,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Printer {
    pub kind: Option<String>,
    pub slicer: Option<String>,
    pub url: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub ftps_port: Option<u16>,
    #[serde(default)]
    pub lan_control: bool,
    #[serde(default)]
    pub start_profile: crate::bambu::StartProfile,
    #[serde(default)]
    pub ams_mapping: BTreeMap<String, u8>,
    pub serial: Option<String>,
    pub access_code: Option<String>,
    pub ca_file: Option<PathBuf>,
    pub cert_sha256: Option<String>,
    pub api_key: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub id: String,
    pub name: String,
    pub model: String,
    pub printer_profile: Option<String>,
    pub print_profile: Option<String>,
    pub filament_profile: Option<String>,
}

impl Config {
    /// Reads only the selected file, refusing exposed secrets before reading its contents.
    pub fn load(path: &Path) -> Result<Self, String> {
        Self::load_mode(path, None)
    }

    /// Enrollment permits absent trust only for the named Bambu printer.
    pub fn load_for_fingerprint(path: &Path, printer: &str) -> Result<Self, String> {
        Self::load_mode(path, Some(printer))
    }

    fn load_mode(path: &Path, enrollment: Option<&str>) -> Result<Self, String> {
        let file = std::fs::File::open(path)
            .map_err(|err| format!("Cannot open config {}: {err}", path.display()))?;
        let metadata = file
            .metadata()
            .map_err(|_| "Cannot check config permissions.")?;
        if !metadata.is_file() {
            return Err("Choose a config file that is a regular file.".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(format!(
                    "Config is accessible to group or others. Run chmod 600 on {}.",
                    path.display()
                ));
            }
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read config file.")?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err("Config exceeds 1 MiB. Use at most 64 printers.".into());
        }
        // Parser diagnostics can quote secret values, so do not pass them on.
        let text = std::str::from_utf8(&bytes).map_err(|_| "Config must be UTF-8 TOML.")?;
        let config: Self = toml::from_str(text)
            .map_err(|_| "Cannot read config TOML. Check the printer fields in docs/print.md.")?;
        config.validate_mode(enrollment)?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        self.validate_mode(None)
    }

    fn validate_mode(&self, enrollment: Option<&str>) -> Result<(), String> {
        if self.printers.len() > MAX_PRINTERS {
            return Err("Use at most 64 printers in the config.".into());
        }
        if !matches!(self.backend.kind.as_str(), "simulated" | "printers") {
            return Err(format!(
                "Backend kind {:?} is not supported yet. Use simulated or printers.",
                self.backend.kind
            ));
        }
        if self.slicer.is_some() && !self.slicers.is_empty() {
            return Err(
                "Use either [slicer] shorthand or [slicers.<name>] tables, not both.".into(),
            );
        }
        if self.slicers.len() > 64 {
            return Err("Use at most 64 slicers.".into());
        }
        for slicer in self.slicers.values() {
            slicer.validate()?;
        }
        if let Some(slicer) = &self.slicer {
            slicer.validate()?;
        }
        if self.backend.slice && self.slicer.is_none() && self.slicers.is_empty() {
            return Err("Set [slicers.<name>] or [slicer] when backend.slice is true.".into());
        }
        let mut ids = HashSet::new();
        for printer in &self.printers {
            if self.backend.kind == "printers" {
                match printer.kind.as_deref() {
                    Some("prusalink") => {
                        crate::prusalink::validate(printer)?;
                    }
                    Some("octoprint") => crate::octoprint::validate(printer)?,
                    Some("bambu") => {
                        crate::bambu::validate_enrollment(
                            printer,
                            enrollment == Some(printer.id.as_str()),
                        )?;
                    }
                    _ => {
                        return Err(
                            "Set each printer kind to prusalink, octoprint, or bambu.".into()
                        )
                    }
                }
                if [
                    &printer.printer_profile,
                    &printer.print_profile,
                    &printer.filament_profile,
                ]
                .iter()
                .any(|p| p.is_none())
                {
                    return Err(
                        "Set printer, print, and filament profiles for each printer.".into(),
                    );
                }
            }
            if self.backend.kind == "printers" || self.backend.slice {
                self.slicer_for(printer)?;
            }
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
            if printer.id.is_empty()
                || printer.id.len() > 64
                || !printer
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                || !ids.insert(&printer.id)
            {
                return Err("Give each printer a unique id of at most 64 letters, digits, hyphens, or underscores.".into());
            }
            if printer.name.trim().is_empty()
                || printer.name.len() > 256
                || printer.name.chars().any(char::is_control)
            {
                return Err("Give each printer a plain name of at most 256 bytes.".into());
            }
            if printer.model.len() > 256 || printer.model.chars().any(char::is_control) {
                return Err("Give each printer a plain model of at most 256 bytes.".into());
            }
        }
        Ok(())
    }
    /// Resolves an explicit name or the sole compatible slicer.
    pub fn slicer_for(&self, printer: &Printer) -> Result<&crate::slicer::Config, String> {
        let kind = match printer.kind.as_deref() {
            Some("prusalink") => Some("prusaslicer"),
            Some("bambu") => Some("orcaslicer"),
            _ => None,
        };
        let selected = if let Some(name) = &printer.slicer {
            self.slicers.get(name).ok_or_else(|| format!(
                "Printer {} names an unknown slicer. Add [slicers.<name>] or change its slicer name.", printer.id
            ))?
        } else {
            let mut candidates = self
                .slicer
                .iter()
                .chain(self.slicers.values())
                .filter(|s| kind.is_none_or(|k| s.kind == k));
            let first = candidates.next().ok_or_else(|| format!(
                "Printer {} needs a slicer of kind {}. Add [slicers.<name>] and select it with slicer.", printer.id, kind.unwrap_or("configured")
            ))?;
            if candidates.next().is_some() {
                return Err(format!(
                    "Printer {} has ambiguous slicer choices. Set its slicer to a configured name.",
                    printer.id
                ));
            }
            first
        };
        if kind.is_some_and(|k| selected.kind != k) {
            return Err(format!(
                "Printer {} requires {}. Choose a slicer of that kind.",
                printer.id,
                kind.unwrap_or_default()
            ));
        }
        Ok(selected)
    }
}

/// The user's platform config folder; this does not read any files.
pub fn default_path() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    base.map(|base| base.join("dutchman-print/config.toml"))
        .ok_or_else(|| "Cannot find your config folder. Set --config FILE.".into())
}
