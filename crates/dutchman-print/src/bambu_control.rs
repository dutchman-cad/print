//! Versioned local project command, using documented protocol facts.
use crate::backend::Error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read as _;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StartProfile {
    pub name: String,
    pub archive_url: String,
    pub md5_target: String,
    pub leveling_field: String,
    pub ams_convention: String,
    pub bed_leveling: bool,
    pub flow_cali: bool,
    pub vibration_cali: bool,
    pub layer_inspect: bool,
}
impl Default for StartProfile {
    fn default() -> Self {
        Self {
            name: "lan-project-1".into(),
            archive_url: "sdcard".into(),
            md5_target: "plate".into(),
            leveling_field: "bed_leveling".into(),
            ams_convention: "per-filament".into(),
            bed_leveling: false,
            flow_cali: false,
            vibration_cali: false,
            layer_inspect: false,
        }
    }
}
impl StartProfile {
    pub fn validate(&self) -> Result<(), String> {
        if self.name != "lan-project-1"
            || !matches!(self.archive_url.as_str(), "sdcard" | "ftp")
            || !matches!(self.md5_target.as_str(), "plate" | "empty")
            || !matches!(
                self.leveling_field.as_str(),
                "bed_leveling" | "bed_levelling"
            )
            || !matches!(
                self.ams_convention.as_str(),
                "per-filament" | "right-aligned-five"
            )
        {
            return Err(
                "Choose the documented lan-project-1 start_profile values in docs/print.md.".into(),
            );
        }
        Ok(())
    }
}
fn refuse(message: &str) -> Error {
    Error::Refused(message.into())
}
fn entry(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, Error> {
    let file = zip
        .by_name(name)
        .map_err(|_| refuse("Sliced 3MF metadata is missing. Prepare and review it again."))?;
    if file.size() > limit {
        return Err(refuse("Sliced 3MF metadata exceeds the size limit."));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| refuse("Cannot read sliced 3MF metadata."))?;
    if bytes.len() as u64 > limit {
        return Err(refuse("Sliced 3MF metadata exceeds the size limit."));
    }
    Ok(bytes)
}
/// Validate slicer-written requirements against the merged, live machine report.
pub(super) fn check_materials(
    config: &crate::config::Printer,
    bytes: &[u8],
    fields: &serde_json::Map<String, Value>,
) -> Result<(), Error> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| refuse("Choose a sliced 3MF archive."))?;
    if zip.len() > 4096 {
        return Err(refuse("Sliced 3MF has too many entries."));
    }
    let settings: Value = serde_json::from_slice(&entry(
        &mut zip,
        "Metadata/project_settings.config",
        1 << 20,
    )?)
    .map_err(|_| refuse("Cannot read sliced settings. Prepare again."))?;
    let gcode = entry(&mut zip, "Metadata/plate_1.gcode", 64 << 20)?;
    let text = std::str::from_utf8(&gcode)
        .map_err(|_| refuse("Use text plate G-code in the sliced 3MF."))?;
    let scalar = |v: &Value| -> Option<String> {
        let v = if let Some(a) = v.as_array() {
            if a.len() != 1 {
                return None;
            }
            a.first()?
        } else {
            v
        };
        v.as_str()
            .map(str::to_owned)
            .or_else(|| v.as_f64().map(|n| n.to_string()))
    };
    let setting = |key: &str| -> Result<Option<String>, Error> {
        let mut found: Option<String> = None;
        let mut record = |value: &str| -> Result<(), Error> {
            if value.len() > 128 || value.chars().any(char::is_control) {
                return Err(refuse("Sliced nozzle setting is invalid. Prepare again."));
            }
            if found.as_deref().is_some_and(|old| old != value) {
                return Err(refuse(
                    "Sliced nozzle settings disagree. Prepare and review again.",
                ));
            }
            if found.is_none() {
                found = Some(value.into());
            }
            Ok(())
        };
        if let Some(v) = settings.get(key) {
            let value = scalar(v).ok_or_else(|| {
                refuse("Sliced nozzle settings are ambiguous. Prepare a single-nozzle profile.")
            })?;
            record(&value)?;
        }
        for line in text.lines() {
            if let Some((k, v)) = line
                .trim()
                .strip_prefix(';')
                .and_then(|l| l.trim().split_once('='))
            {
                if k.trim() == key {
                    record(v.trim())?;
                }
            }
        }
        Ok(found)
    };
    let diameter = setting("nozzle_diameter")?.ok_or_else(|| {
        refuse("Sliced nozzle diameter is missing. Prepare with nozzle_diameter metadata.")
    })?;
    let installed = fields
        .get("nozzle_diameter")
        .and_then(scalar)
        .unwrap_or_else(|| "unknown".into());
    let number = |s: &str| {
        s.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n > 0.0 && *n <= 2.0)
    };
    if number(&diameter).is_none() || number(&diameter) != number(&installed) {
        return Err(Error::Refused(format!("Sliced nozzle diameter {diameter} mm differs from installed nozzle diameter {installed} mm. Fit a matching nozzle or prepare again.")));
    }
    if let Some(required) = setting("nozzle_type")?.filter(|s| !s.is_empty()) {
        let installed = fields
            .get("nozzle_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !required.eq_ignore_ascii_case(installed) {
            return Err(Error::Refused(format!("Sliced nozzle material {required} differs from installed nozzle material {installed}. Fit a matching nozzle or prepare again.")));
        }
    }
    let filaments = settings
        .get("filament_type")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty() && a.len() <= 64)
        .ok_or_else(|| refuse("Sliced filament list is unknown. Prepare again."))?;
    for (index, filament) in filaments.iter().enumerate() {
        let required = filament
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
            .ok_or_else(|| refuse("Sliced filament material is invalid. Prepare again."))?;
        let mapping = config.ams_mapping.get(&(index + 1).to_string());
        let key = match mapping {
            Some(slot) if *slot <= 15 => format!("tray_{slot}"),
            None if filaments.len() == 1 && config.ams_mapping.is_empty() => "external_tray".into(),
            _ => {
                return Err(refuse(
                    "Map every filament with ams_mapping before starting.",
                ))
            }
        };
        let installed = fields
            .get(&key)
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !required.eq_ignore_ascii_case(installed) {
            let location = mapping.map_or_else(
                || "external spool".into(),
                |slot| format!("AMS slot {slot}"),
            );
            return Err(Error::Refused(format!("Sliced filament {} ({required}) differs from {location} material {installed}. Load matching material or prepare again.", index + 1)));
        }
    }
    Ok(())
}

pub(crate) fn start_payload(
    config: &crate::config::Printer,
    filename: &str,
    name: &str,
    bytes: &[u8],
) -> Result<Value, Error> {
    config.start_profile.validate().map_err(Error::Refused)?;
    let p = &config.start_profile;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| refuse("Choose a sliced 3MF archive."))?;
    if zip.len() > 4096 {
        return Err(refuse("Sliced 3MF has too many entries."));
    }
    let gcode = entry(&mut zip, "Metadata/plate_1.gcode", 64 << 20)?;
    let settings = entry(&mut zip, "Metadata/project_settings.config", 1 << 20)?;
    let settings: Value = serde_json::from_slice(&settings)
        .map_err(|_| refuse("Cannot read sliced filament settings. Prepare again."))?;
    let filaments = settings.get("filament_type").and_then(Value::as_array).filter(|f| !f.is_empty() && f.len() <= 64)
        .ok_or_else(|| refuse("Sliced filament list is unknown. Prepare with filament_type metadata before starting."))?;
    if filaments.iter().any(|f| {
        f.as_str()
            .is_none_or(|s| s.len() > 128 || s.chars().any(char::is_control))
    }) {
        return Err(refuse("Use plain filament names of at most 128 bytes."));
    }
    // Tool selections must fit the declared list, even when settings omit unused tools.
    let text = std::str::from_utf8(&gcode)
        .map_err(|_| refuse("Use text plate G-code in the sliced 3MF."))?;
    for line in text.lines() {
        if let Some(tool) = line
            .trim()
            .strip_prefix('T')
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<usize>().ok())
        {
            if tool < 254 && tool >= filaments.len() {
                return Err(refuse(
                    "Plate tool selection exceeds its filament metadata. Prepare and review again.",
                ));
            }
        }
    }
    let use_ams = !config.ams_mapping.is_empty();
    let mut slots = Vec::new();
    let mut missing = Vec::new();
    for (index, filament) in filaments.iter().enumerate() {
        let key = (index + 1).to_string();
        match config.ams_mapping.get(&key) {
            Some(slot) if *slot <= 15 => slots.push(i32::from(*slot)),
            _ if filaments.len() > 1 || use_ams => missing.push(format!(
                "filament {key} ({})",
                filament.as_str().unwrap_or("unknown material")
            )),
            _ => (),
        }
    }
    if !missing.is_empty() {
        return Err(Error::Refused(format!(
            "Map every filament with ams_mapping before starting: {}.",
            missing.join(", ")
        )));
    }
    let mapping = if !use_ams {
        json!("")
    } else if p.ams_convention == "right-aligned-five" {
        if slots.len() > 4 || slots.iter().any(|s| *s > 3) {
            return Err(refuse(
                "The right-aligned-five convention supports at most four filaments and slots 0–3.",
            ));
        }
        let mut padded = vec![-1; 5 - slots.len()];
        padded.extend(slots);
        json!(padded)
    } else {
        json!(slots)
    };
    use md5::{Digest as _, Md5};
    let md5 = match p.md5_target.as_str() {
        "empty" => String::new(),
        _ => Md5::digest(&gcode)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    };
    let url = if p.archive_url == "ftp" {
        format!("ftp:///{filename}")
    } else {
        format!("file:///sdcard/{filename}")
    };
    let mut print = json!({"command":"project_file", "param":"Metadata/plate_1.gcode", "project_id":"0", "profile_id":"0", "task_id":"0", "subtask_id":"0", "subtask_name":name, "url":url, "md5":md5, "timelapse":false, "bed_type":"auto", "flow_cali":p.flow_cali, "vibration_cali":p.vibration_cali, "layer_inspect":p.layer_inspect, "use_ams":use_ams, "ams_mapping":mapping});
    print
        .as_object_mut()
        .ok_or(Error::Unavailable)?
        .insert(p.leveling_field.clone(), json!(p.bed_leveling));
    Ok(json!({"print":print}))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    fn archive(filaments: Value) -> Vec<u8> {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in [
            ("Metadata/plate_1.gcode", b"G1 X0\n".to_vec()),
            (
                "Metadata/project_settings.config",
                json!({"filament_type":filaments}).to_string().into_bytes(),
            ),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
    #[test]
    fn slicer_settings_nozzle_requirements_and_unknown_values() {
        let config = crate::config::Printer::default();
        let fields =
            json!({"nozzle_diameter":"0.4","nozzle_type":"hardened_steel","external_tray":"PLA"})
                .as_object()
                .unwrap()
                .clone();
        let make = |settings: Value, gcode: &str| {
            use std::io::Write as _;
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            for (name, bytes) in [
                ("Metadata/plate_1.gcode", gcode.as_bytes().to_vec()),
                (
                    "Metadata/project_settings.config",
                    settings.to_string().into_bytes(),
                ),
            ] {
                zip.start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(&bytes).unwrap();
            }
            zip.finish().unwrap().into_inner()
        };
        let settings = json!({"filament_type":["PLA"],"nozzle_diameter":["0.4"],"nozzle_type":"hardened_steel"});
        assert!(check_materials(&config, &make(settings.clone(), "G1 X0"), &fields).is_ok());
        assert!(
            check_materials(&config, &make(settings, "; nozzle_diameter = 0.2"), &fields).is_err()
        );
        for diameter in [
            Value::Null,
            json!([]),
            json!(["0.4", "0.6"]),
            json!("NaN"),
            json!("0.2"),
        ] {
            assert!(check_materials(
                &config,
                &make(
                    json!({"filament_type":["PLA"],"nozzle_diameter":diameter}),
                    "G1 X0"
                ),
                &fields
            )
            .is_err());
        }
        assert!(check_materials(&config, &archive(json!(["PLA"])), &fields).is_err());
        let mut missing = fields;
        missing.remove("external_tray");
        assert!(check_materials(
            &config,
            &make(
                json!({"filament_type":["PLA"],"nozzle_diameter":0.4}),
                "G1 X0"
            ),
            &missing
        )
        .is_err());
    }

    #[test]
    fn exact_default_and_all_disputed_options() {
        let mut config = crate::config::Printer::default();
        let bytes = archive(json!(["PLA"]));
        let payload = start_payload(&config, "job.gcode.3mf", "job", &bytes).unwrap();
        assert_eq!(
            payload,
            json!({"print":{"command":"project_file", "param":"Metadata/plate_1.gcode", "project_id":"0", "profile_id":"0", "task_id":"0", "subtask_id":"0", "subtask_name":"job", "url":"file:///sdcard/job.gcode.3mf", "md5":"ff52cbe2fe33af1fa1cdbd00107358b0", "timelapse":false, "bed_type":"auto", "bed_leveling":false, "flow_cali":false, "vibration_cali":false, "layer_inspect":false, "use_ams":false, "ams_mapping":""}})
        );
        config.ams_mapping.insert("1".into(), 3);
        assert_eq!(
            start_payload(&config, "job.gcode.3mf", "job", &bytes).unwrap()["print"]["ams_mapping"],
            json!([3])
        );
        config.start_profile.archive_url = "ftp".into();
        config.start_profile.md5_target = "empty".into();
        config.start_profile.leveling_field = "bed_levelling".into();
        config.start_profile.ams_convention = "right-aligned-five".into();
        config.start_profile.bed_leveling = true;
        config.start_profile.flow_cali = true;
        config.start_profile.vibration_cali = true;
        config.start_profile.layer_inspect = true;
        let mut expected = payload;
        let print = expected["print"].as_object_mut().unwrap();
        print.remove("bed_leveling");
        for (key, value) in [
            ("bed_levelling", json!(true)),
            ("flow_cali", json!(true)),
            ("vibration_cali", json!(true)),
            ("layer_inspect", json!(true)),
            ("url", json!("ftp:///job.gcode.3mf")),
            ("md5", json!("")),
            ("use_ams", json!(true)),
            ("ams_mapping", json!([-1, -1, -1, -1, 3])),
        ] {
            print.insert(key.into(), value);
        }
        assert_eq!(
            start_payload(&config, "job.gcode.3mf", "job", &bytes).unwrap(),
            expected
        );
        config.start_profile.name = "unknown".into();
        assert!(start_payload(&config, "job.gcode.3mf", "job", &bytes).is_err());
    }
    #[test]
    fn every_missing_filament_is_named() {
        let mut config = crate::config::Printer::default();
        let bytes = archive(json!(["PLA", "PETG", "ABS"]));
        let error = start_payload(&config, "job.gcode.3mf", "job", &bytes)
            .unwrap_err()
            .to_string();
        for name in ["filament 1 (PLA)", "filament 2 (PETG)", "filament 3 (ABS)"] {
            assert!(error.contains(name));
        }
        config.ams_mapping.insert("1".into(), 3);
        let error = start_payload(&config, "job.gcode.3mf", "job", &bytes)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("filament 1 (PLA)"));
        config.ams_mapping.insert("2".into(), 0);
        config.ams_mapping.insert("3".into(), 1);
        assert_eq!(
            start_payload(&config, "job.gcode.3mf", "job", &bytes).unwrap()["print"]["ams_mapping"],
            json!([3, 0, 1])
        );
        config.start_profile.ams_convention = "right-aligned-five".into();
        assert_eq!(
            start_payload(&config, "job.gcode.3mf", "job", &bytes).unwrap()["print"]["ams_mapping"],
            json!([-1, -1, 3, 0, 1])
        );
        config.ams_mapping.insert("1".into(), 7);
        assert!(start_payload(&config, "job.gcode.3mf", "job", &bytes).is_err());
        config.start_profile.ams_convention = "per-filament".into();
        assert!(start_payload(&config, "job.gcode.3mf", "job", &bytes).is_ok());
    }
}
