//! Only the Rust fake slicer runs here, regardless of installed slicers.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use dutchman_print::backend::{Backend, Options};
use dutchman_print::config::Printer;
use dutchman_print::slicer::{Config, OutputKind, Slicer};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn config(dir: &Path, kind: &str) -> Config {
    Config {
        kind: kind.into(),
        path: Some(PathBuf::from(env!("CARGO_BIN_EXE_dutchman-fake-slicer"))),
        datadir: Some(dir.join("presets")),
        timeout_seconds: 10,
    }
}
fn printer() -> Printer {
    Printer {
        id: "demo".into(),
        name: "Demo".into(),
        model: "Example".into(),
        printer_profile: Some("Machine".into()),
        print_profile: Some("Draft".into()),
        filament_profile: Some("PLA".into()),
    }
}
fn input(dir: &Path) -> PathBuf {
    use std::io::Write as _;
    let path = dir.join("input.3mf");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    zip.start_file("3D/demo.model", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"<model/>").unwrap();
    zip.finish().unwrap();
    path
}
fn expected(dir: &Path, kind: &str) {
    let mut args = vec![format!("--datadir={}", dir.join("presets").display())];
    if kind == "prusaslicer" {
        args.extend([
            "--export-gcode".into(),
            "--no-binary-gcode".into(),
            format!("--output={}", dir.join("sliced.gcode").display()),
            "--printer-profile=Machine".into(),
            "--print-profile=Fine ; profile".into(),
            "--material-profile=PLA".into(),
        ]);
    } else {
        for (category, name) in [
            ("machine", "Machine"),
            ("process", "Fine ; profile"),
            ("filament", "PLA"),
        ] {
            let path = dir.join("presets/user/default").join(category);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("unrelated-filename.json"),
                serde_json::json!({"name":name}).to_string(),
            )
            .unwrap();
        }
        args.extend([
            format!(
                "--load-filaments={}",
                dir.join("presets/user/default/filament/unrelated-filename.json")
                    .display()
            ),
            format!(
                "--load-settings={};{}",
                dir.join("presets/user/default/machine/unrelated-filename.json")
                    .display(),
                dir.join("presets/user/default/process/unrelated-filename.json")
                    .display()
            ),
            "--slice=0".into(),
            "--export-3mf=sliced.3mf".into(),
            format!("--outputdir={}", dir.display()),
        ]);
    }
    args.push(dir.join("input.3mf").display().to_string());
    std::fs::write(
        dir.join("expected.json"),
        serde_json::to_vec(&args).unwrap(),
    )
    .unwrap();
}
fn options() -> Options {
    Options {
        print_profile: Some("Fine ; profile".into()),
        // Job names remain metadata and must never become slicer arguments.
        name: Some("--post-process=metadata-only".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn commands_outputs_profiles_and_previews() {
    for kind in ["prusaslicer", "orcaslicer"] {
        for output in [OutputKind::Gcode, OutputKind::ThreeMf] {
            let dir = tempfile::tempdir().unwrap();
            let dir = dir.path().canonicalize().unwrap();
            let input = input(&dir);
            expected(&dir, kind);
            let prepared = Slicer::new(config(&dir, kind))
                .unwrap()
                .prepare(&input, &printer(), &options(), output)
                .await
                .unwrap();
            assert_eq!(prepared.estimate.seconds, Some(123));
            assert_eq!(prepared.estimate.filament_mm, Some(450.0));
            assert_eq!(prepared.estimate.filament_g, Some(1.5));
            assert_eq!(prepared.estimate.layers, Some(12));
            assert!(std::fs::read(prepared.estimate.preview.unwrap())
                .unwrap()
                .starts_with(b"\x89PNG"));
            if output == OutputKind::ThreeMf {
                let mut zip =
                    zip::ZipArchive::new(std::fs::File::open(prepared.path).unwrap()).unwrap();
                assert!(zip.by_name("Metadata/plate_1.gcode").is_ok());
            } else {
                assert!(std::fs::read_to_string(prepared.path)
                    .unwrap()
                    .contains("G1 X0"));
            }
        }
    }
}
#[tokio::test]
async fn failures_are_bounded_and_missing_profiles_name_folders() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let input = input(&dir);
    expected(&dir, "prusaslicer");
    std::fs::write(dir.join("mode"), "fail").unwrap();
    let error = Slicer::new(config(&dir, "prusaslicer"))
        .unwrap()
        .prepare(&input, &printer(), &options(), OutputKind::Gcode)
        .await
        .err()
        .unwrap();
    assert!(error.contains("last stderr") && error.contains("last stdout"));
    assert!(error.len() < 17000);
    let error = Slicer::new(config(&dir, "orcaslicer"))
        .unwrap()
        .prepare(&input, &printer(), &options(), OutputKind::Gcode)
        .await
        .err()
        .unwrap();
    assert!(
        error.contains("Machine")
            && error.contains(&dir.join("presets/user").display().to_string())
            && error.contains(&dir.join("presets/system").display().to_string())
    );
}
#[test]
fn missing_executable_and_unknown_kind() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), "prusaslicer");
    cfg.path = Some(dir.path().join("never-installed"));
    let error = Slicer::new(cfg.clone()).err().unwrap();
    assert!(error.contains("slicer.path") && error.contains("prusaslicer"));
    cfg.kind = "future-slicer".into();
    assert!(Slicer::new(cfg).err().unwrap().contains("future-slicer"));
}
#[tokio::test]
async fn timeout_and_drop_kill_the_child() {
    for drop_request in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let input = input(&dir);
        expected(&dir, "prusaslicer");
        std::fs::write(dir.join("mode"), "sleep").unwrap();
        let mut cfg = config(&dir, "prusaslicer");
        cfg.timeout_seconds = 1;
        let handle = tokio::spawn(async move {
            Slicer::new(cfg)
                .unwrap()
                .prepare(&input, &printer(), &options(), OutputKind::Gcode)
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !dir.join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if drop_request {
            handle.abort();
            let _ = handle.await;
        } else {
            let error = handle.await.unwrap().err().unwrap();
            assert!(error.contains("timed out") && error.contains("waiting for slice"));
        }
        tokio::time::sleep(Duration::from_millis(3200)).await;
        assert!(!dir.join("finished").exists());
    }
}
#[tokio::test]
async fn simulated_backend_slices_when_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let input = input(&dir);
    expected(&dir, "prusaslicer");
    let cfg = dutchman_print::config::Config {
        backend: dutchman_print::config::Backend {
            kind: "simulated".into(),
            slice: true,
        },
        slicer: Some(config(&dir, "prusaslicer")),
        printers: vec![printer()],
    };
    let backend = dutchman_print::simulated::Simulated::new(
        cfg,
        Arc::new(dutchman_print::simulated::SystemClock::default()),
    )
    .unwrap();
    let estimate = backend
        .prepare("test", &input, "demo", options())
        .await
        .unwrap();
    assert_eq!(estimate.seconds, Some(123));
    assert!(estimate.preview.is_some());
    assert_eq!(backend.job("test").await.unwrap().estimate.layers, Some(12));
}

#[tokio::test]
async fn orca_system_presets_and_ambiguous_names() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let input = input(&dir);
    expected(&dir, "orcaslicer");
    let old = dir.join("presets/user/default/machine");
    let new = dir.join("presets/system/Example/machine");
    std::fs::create_dir_all(new.parent().unwrap()).unwrap();
    std::fs::rename(&old, &new).unwrap();
    let args = std::fs::read_to_string(dir.join("expected.json")).unwrap();
    let mut args: Vec<String> = serde_json::from_str(&args).unwrap();
    for arg in &mut args {
        *arg = arg.replace(&old.display().to_string(), &new.display().to_string());
    }
    std::fs::write(
        dir.join("expected.json"),
        serde_json::to_vec(&args).unwrap(),
    )
    .unwrap();
    let slicer = Slicer::new(config(&dir, "orcaslicer")).unwrap();
    assert!(slicer
        .prepare(&input, &printer(), &options(), OutputKind::Gcode)
        .await
        .is_ok());
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("duplicate.json"), r#"{"name":"Machine"}"#).unwrap();
    let error = slicer
        .prepare(&input, &printer(), &options(), OutputKind::Gcode)
        .await
        .err()
        .unwrap();
    assert!(error.contains("ambiguous") && error.contains("Machine"));
}

#[test]
fn config_defaults_and_requirements() {
    let text = "[backend]\nkind = 'simulated'\nslice = true\n[slicer]\nkind = 'prusaslicer'\n";
    let cfg: dutchman_print::config::Config = toml::from_str(text).unwrap();
    let slicer = cfg.slicer.unwrap();
    assert_eq!(slicer.timeout_seconds, 600);
    assert!(slicer.path.is_none() && slicer.datadir.is_none());
    let cfg = toml::from_str("[backend]\nkind = 'simulated'\nslice = true\n").unwrap();
    assert!(dutchman_print::simulated::Simulated::new(
        cfg,
        Arc::new(dutchman_print::simulated::SystemClock::default())
    )
    .err()
    .unwrap()
    .contains("[slicer]"));
}

#[tokio::test]
async fn missing_estimates_stay_absent() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let input = input(&dir);
    expected(&dir, "prusaslicer");
    std::fs::write(dir.join("mode"), "missing").unwrap();
    let prepared = Slicer::new(config(&dir, "prusaslicer"))
        .unwrap()
        .prepare(&input, &printer(), &options(), OutputKind::Gcode)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(prepared.estimate).unwrap(),
        serde_json::json!({})
    );
}

#[tokio::test]
async fn slicer_refuses_option_like_profiles_before_running() {
    let dir = tempfile::tempdir().unwrap();
    for kind in ["prusaslicer", "orcaslicer"] {
        let slicer = Slicer::new(config(dir.path(), kind)).unwrap();
        for field in 0..3 {
            for in_options in [false, true] {
                let mut printer = printer();
                let mut options = Options::default();
                let profiles = if in_options {
                    [
                        &mut options.printer_profile,
                        &mut options.print_profile,
                        &mut options.filament_profile,
                    ]
                } else {
                    [
                        &mut printer.printer_profile,
                        &mut printer.print_profile,
                        &mut printer.filament_profile,
                    ]
                };
                *profiles.into_iter().nth(field).unwrap() = Some("--post-process=command".into());
                let error = slicer
                    .prepare(
                        &dir.path().join("not-read.3mf"),
                        &printer,
                        &options,
                        OutputKind::Gcode,
                    )
                    .await
                    .err()
                    .unwrap();
                assert_eq!(error, "Profile names can't start with '-'.");
                assert!(!dir.path().join("started").exists());
            }
        }
    }
}
