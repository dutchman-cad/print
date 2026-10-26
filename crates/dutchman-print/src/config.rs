//! Printer settings. Secrets are never serializable or included in diagnostics.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const MAX_CONFIG_BYTES: u64 = 1 << 20;
pub const MAX_PRINTERS: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub backend: Backend,
    #[serde(default)]
    pub printers: Vec<Printer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub kind: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Printer {
    pub id: String,
    pub name: String,
    pub model: String,
}

impl Config {
    /// Reads only the selected file, refusing exposed secrets before reading its contents.
    pub fn load(path: &Path) -> Result<Self, String> {
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
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.printers.len() > MAX_PRINTERS {
            return Err("Use at most 64 printers in the config.".into());
        }
        if self.backend.kind != "simulated" {
            return Err(format!(
                "Backend kind {:?} is not supported yet. Use simulated.",
                self.backend.kind
            ));
        }
        let mut ids = HashSet::new();
        for printer in &self.printers {
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
