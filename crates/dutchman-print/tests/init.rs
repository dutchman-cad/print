//! `init` writes a private config that loads, without contacting anything.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dutchman_print::config::Config;

const SECRET: &str = "init-test-secret-7f3a";
const FINGERPRINT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Runs `dutchman-print init` without a terminal, with every config folder inside `dir`.
fn init(dir: &Path, args: &[&str]) -> Output {
    let home = dir.join("home");
    Command::new(env!("CARGO_BIN_EXE_dutchman-print"))
        .arg("init")
        .args(args)
        .env_remove("DUTCHMAN_PRINT_CONFIG")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("APPDATA", home.join("appdata"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A loopback port this test owns; any connection to it would be visible.
fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    (listener, port)
}

fn assert_untouched(listener: &TcpListener) {
    match listener.accept() {
        Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock),
        Ok(_) => panic!("init connected to the printer address"),
    }
}

#[cfg(unix)]
fn assert_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{}", path.display());
}

#[cfg(not(unix))]
fn assert_private(_: &Path) {}

#[test]
fn every_kind_writes_a_private_config_that_loads_without_contacting_anything() {
    let (listener, port) = listener();
    let url = format!("http://127.0.0.1:{port}");
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("simulated", vec![]),
        ("prusalink", vec!["--url", &url, "--api-key", SECRET]),
        (
            "prusalink",
            vec!["--url", &url, "--username", "maker", "--password", SECRET],
        ),
        (
            "octoprint",
            vec!["--url", &url, "--api-key", SECRET, "--slicer", "orcaslicer"],
        ),
        ("moonraker", vec!["--url", &url, "--api-key", SECRET]),
        ("snapmaker-u1", vec!["--url", &url]),
        (
            "duet",
            vec![
                "--url",
                &url,
                "--password",
                SECRET,
                "--nozzle-diameter",
                "0.4",
            ],
        ),
        ("duet-sbc", vec!["--url", &url, "--nozzle-diameter", "0.4"]),
        (
            "repetier",
            vec![
                "--url",
                &url,
                "--api-key",
                SECRET,
                "--slug",
                "workshop_mk3",
                "--nozzle-diameter",
                "0.4",
            ],
        ),
        (
            "bambu",
            vec![
                "--host",
                "127.0.0.1",
                "--port",
                &port,
                "--serial",
                "01S00A000000000",
                "--access-code",
                SECRET,
                "--cert-sha256",
                FINGERPRINT,
            ],
        ),
        (
            "elegoo-sdcp",
            vec!["--host", "127.0.0.1", "--port", &port, "--profile", "resin"],
        ),
        (
            "elegoo-sdcp",
            vec!["--host", "127.0.0.1", "--profile", "centauri"],
        ),
        ("snapmaker-2", vec!["--host", "127.0.0.1", "--port", &port]),
        ("snapmaker-j1", vec!["--host", "127.0.0.1", "--port", &port]),
    ];
    for (kind, args) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut all = vec!["--kind", kind, "--output", path.to_str().unwrap()];
        all.extend(args);
        let out = init(dir.path(), &all);
        let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
        assert!(out.status.success(), "{kind}: {stderr}");
        assert!(
            !stdout.contains(SECRET) && !stderr.contains(SECRET),
            "{kind}"
        );
        assert!(stdout.contains(" printers\n"), "{kind}: {stdout}");
        assert!(stdout.contains("--config"), "{kind}: {stdout}");
        if kind != "simulated" {
            assert!(stdout.contains("Nothing has been sent to the printer"));
        }
        assert_private(&path);
        let config = Config::load(&path).unwrap_or_else(|err| panic!("{kind}: {err}"));
        assert_eq!(config.printers.len(), 1);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.lines().filter(|l| l.starts_with('#')).count() > 5);
        if kind == "simulated" {
            assert_eq!(config.backend.kind, "simulated");
        } else {
            assert_eq!(config.printers[0].kind.as_deref(), Some(kind));
        }
    }
    assert_untouched(&listener);
}

#[test]
fn simulated_config_lists_its_printer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let out = init(
        dir.path(),
        &["--kind", "simulated", "--output", path.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let listed = Command::new(env!("CARGO_BIN_EXE_dutchman-print"))
        .arg("--config")
        .arg(&path)
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .arg("printers")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(listed.status.success(), "{}", text(&listed.stderr));
    assert_eq!(text(&listed.stdout), "Workbench demo: idle\n");
}

#[test]
fn bambu_without_trust_loads_for_fingerprint_and_keeps_control_off() {
    let (listener, port) = listener();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let out = init(
        dir.path(),
        &[
            "--kind",
            "bambu",
            "--host",
            "127.0.0.1",
            "--port",
            &port,
            "--serial",
            "01S00A000000000",
            "--access-code",
            SECRET,
            "--id",
            "a1",
            "--output",
            path.to_str().unwrap(),
        ],
    );
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(stdout.contains("fingerprint a1"), "{stdout}");
    assert!(!stdout.contains(SECRET) && !text(&out.stderr).contains(SECRET));
    Config::load_for_fingerprint(&path, "a1").unwrap();
    let refused = Config::load(&path).err().unwrap();
    assert!(refused.contains("ca_file or cert_sha256"), "{refused}");
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("\nlan_control = false\n"));
    assert!(written.contains("experimental"));
    assert!(written.contains("[slicer]\nkind = \"orcaslicer\"\n"));
    assert_untouched(&listener);
}

#[test]
fn monitoring_only_kinds_say_so() {
    for (kind, extra) in [
        ("elegoo-sdcp", &["--profile", "centauri"][..]),
        ("snapmaker-2", &[][..]),
        ("snapmaker-j1", &[][..]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut args = vec![
            "--kind",
            kind,
            "--host",
            "printer.local",
            "--output",
            path.to_str().unwrap(),
        ];
        args.extend(extra);
        assert!(init(dir.path(), &args).status.success());
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("Monitoring only"), "{kind}");
        assert!(!written.contains("[slicer]"), "{kind}");
    }
}

#[test]
fn existing_files_are_kept_unless_forced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "keep me").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let args = ["--kind", "simulated", "--output", path.to_str().unwrap()];
    let out = init(dir.path(), &args);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("--force"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");

    let mut forced = args.to_vec();
    forced.push("--force");
    let out = init(dir.path(), &forced);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_private(&path);
    Config::load(&path).unwrap();
    let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(leftovers, 1, "only config.toml remains");
}

#[test]
fn missing_answers_name_the_flag_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let output = path.to_str().unwrap();
    for (args, flag) in [
        (vec![], "--kind"),
        (vec!["--kind", "prusalink"], "--url"),
        (
            vec!["--kind", "prusalink", "--url", "http://printer.local"],
            "--api-key",
        ),
        (
            vec![
                "--kind",
                "prusalink",
                "--url",
                "http://printer.local",
                "--username",
                "maker",
            ],
            "--password",
        ),
        (
            vec!["--kind", "octoprint", "--url", "http://printer.local"],
            "--api-key",
        ),
        (
            vec!["--kind", "duet", "--url", "http://printer.local"],
            "--nozzle-diameter",
        ),
        (
            vec!["--kind", "bambu", "--host", "printer.local"],
            "--serial",
        ),
        (
            vec![
                "--kind",
                "bambu",
                "--host",
                "printer.local",
                "--serial",
                "01S00A000000000",
            ],
            "--access-code",
        ),
        (vec!["--kind", "elegoo-sdcp"], "--profile"),
        (vec!["--kind", "snapmaker-2"], "--host"),
    ] {
        let mut all = args.clone();
        all.extend(["--output", output]);
        let out = init(dir.path(), &all);
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            stderr.contains(&format!("Missing {flag}")),
            "{args:?}: {stderr}"
        );
        assert!(!path.exists());
    }
}

#[test]
fn wrong_answers_are_refused_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let output = path.to_str().unwrap();
    for (args, message) in [
        (vec!["--kind", "cura"], "Set --kind to one of"),
        (
            vec!["--kind", "bambu", "--url", "http://printer.local"],
            "Use --host",
        ),
        (
            vec!["--kind", "snapmaker-2", "--host", "a", "--api-key", SECRET],
            "--api-key does not apply",
        ),
        (
            vec![
                "--kind",
                "prusalink",
                "--url",
                "https://printer.local",
                "--api-key",
                SECRET,
            ],
            "http://host[:port]",
        ),
        (
            vec![
                "--kind",
                "prusalink",
                "--url",
                "http://printer.local",
                "--api-key",
                SECRET,
                "--slicer",
                "orcaslicer",
            ],
            "need prusaslicer",
        ),
        (
            vec![
                "--kind",
                "moonraker",
                "--url",
                "http://printer.local",
                "--id",
                "has space",
            ],
            "unique id",
        ),
        (
            vec![
                "--kind",
                "duet",
                "--url",
                "http://duet.local",
                "--nozzle-diameter",
                "11",
            ],
            "at most 10",
        ),
        (
            vec![
                "--kind",
                "repetier",
                "--url",
                "http://repetier.local",
                "--api-key",
                "key",
                "--nozzle-diameter",
                "0.4",
            ],
            "Missing --slug",
        ),
        (
            vec![
                "--kind",
                "octoprint",
                "--url",
                "http://octoprint.local",
                "--slug",
                "mk3",
            ],
            "--slug does not apply",
        ),
    ] {
        let mut all = args.clone();
        all.extend(["--output", output]);
        let out = init(dir.path(), &all);
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{args:?}");
        assert!(stderr.contains(message), "{args:?}: {stderr}");
        assert!(!stderr.contains(SECRET));
        assert!(!path.exists());
    }
}

/// The path other commands read when no --config is given, for the environment `init` sets.
fn default_config(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    if cfg!(target_os = "windows") {
        home.join("appdata")
    } else if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        home.join("xdg")
    }
    .join("dutchman-print")
    .join("config.toml")
}

#[test]
fn default_and_global_config_paths_are_used() {
    let dir = tempfile::tempdir().unwrap();
    let out = init(dir.path(), &["--kind", "simulated"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let path = default_config(dir.path());
    Config::load(&path).unwrap();
    assert_private(&path);
    let stdout = text(&out.stdout);
    assert!(stdout.contains("  dutchman-print printers\n"), "{stdout}");

    let chosen = dir.path().join("chosen.toml");
    let out = Command::new(env!("CARGO_BIN_EXE_dutchman-print"))
        .arg("--config")
        .arg(&chosen)
        .args(["init", "--kind", "simulated"])
        .env_remove("DUTCHMAN_PRINT_CONFIG")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    Config::load(&chosen).unwrap();
}
