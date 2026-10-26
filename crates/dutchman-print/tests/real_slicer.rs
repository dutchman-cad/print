//! Manual Linux Flatpak check. Builds its own model and disables network and devices.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use dutchman_print::backend::Options;
use dutchman_print::config::Printer;
use dutchman_print::slicer::{Config, OutputKind, Slicer};
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

fn cube(path: &Path) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let files = [
        ("[Content_Types].xml", r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/></Types>"#.to_string()),
        ("_rels/.rels", r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/></Relationships>"#.to_string()),
        ("3D/3dmodel.model", {
            let mut model = String::from(r#"<?xml version="1.0"?><model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"><resources><object id="1" type="model"><mesh><vertices>"#);
            for (x,y,z) in [(0,0,0),(20,0,0),(20,20,0),(0,20,0),(0,0,20),(20,0,20),(20,20,20),(0,20,20)] {
                model.push_str(&format!(r#"<vertex x="{x}" y="{y}" z="{z}"/>"#));
            }
            model.push_str("</vertices><triangles>");
            for (a,b,c) in [(0,2,1),(0,3,2),(4,5,6),(4,6,7),(0,1,5),(0,5,4),(1,2,6),(1,6,5),(2,3,7),(2,7,6),(3,0,4),(3,4,7)] {
                model.push_str(&format!(r#"<triangle v1="{a}" v2="{b}" v3="{c}"/>"#));
            }
            model.push_str("</triangles></mesh></object></resources><build><item objectid=\"1\" transform=\"1 0 0 0 1 0 0 0 1 80 80 0\"/></build></model>");
            model
        }),
    ];
    for (name, text) in files {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

fn copy_presets(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_presets(&entry.path(), &to.join(entry.file_name()));
        } else if entry.path().extension().is_some_and(|e| e == "json") {
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

#[tokio::test]
#[ignore = "requires an installed Flatpak and DUTCHMAN_PRINT_REAL_SLICER"]
async fn real_slicer() {
    let kind = std::env::var("DUTCHMAN_PRINT_REAL_SLICER").unwrap();
    let id = match kind.as_str() {
        "prusaslicer" => "com.prusa3d.PrusaSlicer",
        "orcaslicer" => "io.github.softfever.OrcaSlicer",
        _ => panic!("Choose prusaslicer or orcaslicer"),
    };
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    if std::env::var_os("DUTCHMAN_PRINT_REAL_SLICER_KEEP").is_some() {
        let _ = temp.keep();
        eprintln!("Artifacts: {}", dir.display());
    }
    assert!(
        dir.starts_with(std::fs::canonicalize(std::env::var_os("HOME").unwrap()).unwrap()),
        "Set TMPDIR below HOME for Flatpak access"
    );
    let executable = if kind == "prusaslicer" {
        "prusa-slicer"
    } else {
        "orca-slicer"
    };
    if std::env::var_os("DUTCHMAN_PRINT_REAL_SLICER_HELP").is_some() {
        let help = std::process::Command::new("timeout")
            .current_dir(&dir)
            .args([
                "20s",
                "flatpak",
                "run",
                "--unshare=network",
                "--nodevice=all",
                &format!("--command={executable}"),
                id,
                "--help",
            ])
            .output()
            .unwrap();
        eprintln!(
            "{}\n{}",
            String::from_utf8_lossy(&help.stdout),
            String::from_utf8_lossy(&help.stderr)
        );
        assert!(help.status.success());
        return;
    }
    let diagnostic = if kind == "orcaslicer" {
        "--export-settings \"$PWD/effective.json\""
    } else {
        ""
    };
    let wrapper = dir.join("offline-slicer");
    std::fs::write(&wrapper, format!("#!/bin/sh\nexec flatpak run --unshare=network --nodevice=all --command={executable} {id} {diagnostic} \"$@\"\n")).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let datadir = dir.join("presets");
    std::fs::create_dir_all(&datadir).unwrap();
    let (machine, process, filament) = if kind == "prusaslicer" {
        std::fs::write(
            datadir.join("PrusaSlicer.ini"),
            "version = 2.9.6\nno_defaults = 1\n[vendor:PrusaResearch]\nmodel:MINI = 0.4\n[filaments]\nPrusament PLA = 1\n",
        )
        .unwrap();
        let vendor = datadir.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::copy(
            format!("/var/lib/flatpak/app/{id}/current/active/files/share/PrusaSlicer/profiles/PrusaResearch.ini"),
            vendor.join("PrusaResearch.ini"),
        ).unwrap();
        (
            "Original Prusa MINI & MINI+",
            "0.20mm QUALITY @MINI",
            "Prusament PLA",
        )
    } else {
        let profiles = std::path::PathBuf::from(format!(
            "/var/lib/flatpak/app/{id}/current/active/files/share/OrcaSlicer/profiles"
        ));
        copy_presets(&profiles, &datadir.join("system"));
        if std::env::var_os("DUTCHMAN_PRINT_REAL_SLICER_ORCA_LAYER_RESET").is_some() {
            let path = datadir.join("system/BBL/machine/Bambu Lab A1 0.4 nozzle.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert("before_layer_change_gcode".into(), "G92 E0".into());
            std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            eprintln!("Temporary Orca preset override: before_layer_change_gcode = G92 E0");
        }
        (
            "Bambu Lab A1 0.4 nozzle",
            "0.20mm Standard @BBL A1",
            "Generic PLA @BBL A1",
        )
    };
    let printer = Printer {
        id: "cube".into(),
        name: "Cube".into(),
        model: machine.into(),
        printer_profile: Some(machine.into()),
        print_profile: Some(process.into()),
        filament_profile: Some(filament.into()),
        ..Default::default()
    };
    let input = dir.join("cube.3mf");
    cube(&input);
    let slicer = Slicer::new(Config {
        kind: kind.clone(),
        path: Some(wrapper),
        datadir: Some(datadir),
        timeout_seconds: 180,
    })
    .unwrap();
    for output in [OutputKind::Gcode, OutputKind::ThreeMf] {
        let result = slicer
            .prepare(&input, &printer, &Options::default(), output)
            .await
            .unwrap();
        if kind == "prusaslicer" {
            let gcode = std::fs::read_to_string(dir.join("sliced.gcode")).unwrap();
            for (key, name) in [
                ("printer_settings_id", machine),
                ("print_settings_id", process),
                ("filament_settings_id", filament),
            ] {
                assert!(gcode
                    .lines()
                    .any(|line| line.starts_with(&format!("; {key} = ")) && line.contains(name)));
            }
        }
        assert_eq!(
            result.path.extension().unwrap(),
            if output == OutputKind::Gcode {
                "gcode"
            } else {
                "3mf"
            }
        );
        if output == OutputKind::ThreeMf {
            let mut zip = zip::ZipArchive::new(std::fs::File::open(&result.path).unwrap()).unwrap();
            assert!(zip.by_name("Metadata/plate_1.gcode").unwrap().size() > 0);
        } else {
            assert!(std::fs::metadata(&result.path).unwrap().len() > 0);
        }
        eprintln!(
            "{kind}: {machine:?}, {process:?}, {filament:?}: {}",
            serde_json::to_string(&result.estimate).unwrap()
        );
        assert!(result.estimate.seconds.is_some_and(|v| v > 0));
        assert!(result.estimate.filament_mm.is_some_and(|v| v > 0.0));
        assert!(result.estimate.filament_g.is_some_and(|v| v > 0.0));
    }
}
