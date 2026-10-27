//! `init`: writes a commented config for one printer. It never contacts a printer, the
//! network, or a slicer; it only writes the file after the config loader accepts it.

use std::io::{BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};

const MAX_ANSWER: u64 = 4096;

/// Answers for `init`. Each one left out is asked on a terminal.
#[derive(clap::Args, Default)]
pub struct Flags {
    /// Printer kind, such as prusalink, moonraker, or bambu.
    #[arg(long)]
    pub kind: Option<String>,
    /// Printer address for PrusaLink, OctoPrint, Moonraker, Snapmaker U1, and Duet.
    #[arg(long)]
    pub url: Option<String>,
    /// Hostname or IP address for Bambu, Elegoo, Snapmaker 2.0, and J1 printers.
    #[arg(long)]
    pub host: Option<String>,
    /// Port, only when a Bambu, Elegoo, or Snapmaker 2.0 or J1 printer uses another one.
    #[arg(long)]
    pub port: Option<u16>,
    /// API key for PrusaLink, OctoPrint, Moonraker, or Snapmaker U1.
    #[arg(long)]
    pub api_key: Option<String>,
    /// PrusaLink login, used instead of an API key.
    #[arg(long)]
    pub username: Option<String>,
    /// PrusaLink login password, or the Duet machine password.
    #[arg(long)]
    pub password: Option<String>,
    /// Bambu printer serial number.
    #[arg(long)]
    pub serial: Option<String>,
    /// Bambu LAN access code.
    #[arg(long)]
    pub access_code: Option<String>,
    /// Bambu certificate fingerprint, 64 hexadecimal digits.
    #[arg(long)]
    pub cert_sha256: Option<String>,
    /// Bambu CA file, used instead of a fingerprint.
    #[arg(long)]
    pub ca_file: Option<PathBuf>,
    /// Elegoo protocol profile: centauri or resin.
    #[arg(long)]
    pub profile: Option<String>,
    /// Installed nozzle diameter in millimetres (Duet and Repetier-Server).
    #[arg(long)]
    pub nozzle_diameter: Option<f64>,
    /// Repetier-Server printer slug, from the server's printer settings.
    #[arg(long)]
    pub slug: Option<String>,
    /// Slicer: prusaslicer or orcaslicer.
    #[arg(long)]
    pub slicer: Option<String>,
    /// Slicer printer preset name.
    #[arg(long)]
    pub printer_profile: Option<String>,
    /// Slicer print preset name.
    #[arg(long)]
    pub print_profile: Option<String>,
    /// Slicer filament preset name.
    #[arg(long)]
    pub filament_profile: Option<String>,
    /// Short printer id for commands.
    #[arg(long)]
    pub id: Option<String>,
    /// Printer name shown in lists.
    #[arg(long)]
    pub name: Option<String>,
    /// Printer model, as free text.
    #[arg(long)]
    pub model: Option<String>,
    /// File to write (default: the config path other commands read).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Replace an existing file.
    #[arg(long)]
    pub force: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Address {
    None,
    Url,
    Host,
}

#[derive(Clone, Copy, PartialEq)]
enum Slicing {
    None,
    Prusa,
    Orca,
    Either,
}

struct Kind {
    name: &'static str,
    about: &'static str,
    address: Address,
    slicing: Slicing,
    model: &'static str,
}

const KINDS: [Kind; 11] = [
    Kind {
        name: "simulated",
        about: "A built-in demo printer for trying Print. Contacts nothing.",
        address: Address::None,
        slicing: Slicing::None,
        model: "Example printer",
    },
    Kind {
        name: "prusalink",
        about: "Prusa printers with PrusaLink, such as MINI, MK4, and XL.",
        address: Address::Url,
        slicing: Slicing::Prusa,
        model: "Prusa printer",
    },
    Kind {
        name: "octoprint",
        about: "A printer run by OctoPrint.",
        address: Address::Url,
        slicing: Slicing::Either,
        model: "OctoPrint printer",
    },
    Kind {
        name: "moonraker",
        about: "Klipper printers with Moonraker, such as Mainsail or Fluidd setups.",
        address: Address::Url,
        slicing: Slicing::Either,
        model: "Klipper printer",
    },
    Kind {
        name: "snapmaker-u1",
        about: "Snapmaker U1.",
        address: Address::Url,
        slicing: Slicing::Either,
        model: "Snapmaker U1",
    },
    Kind {
        name: "duet",
        about: "Duet boards running RepRapFirmware without a single-board computer.",
        address: Address::Url,
        slicing: Slicing::Either,
        model: "Duet printer",
    },
    Kind {
        name: "repetier",
        about: "A printer run by Repetier-Server.",
        address: Address::Url,
        slicing: Slicing::Either,
        model: "Repetier-Server printer",
    },
    Kind {
        name: "bambu",
        about: "Bambu Lab printers in LAN mode. Monitoring; control is experimental.",
        address: Address::Host,
        slicing: Slicing::Orca,
        model: "Bambu Lab printer",
    },
    Kind {
        name: "elegoo-sdcp",
        about: "Elegoo Centauri Carbon and resin printers. Monitoring only.",
        address: Address::Host,
        slicing: Slicing::None,
        model: "Elegoo printer",
    },
    Kind {
        name: "snapmaker-2",
        about: "Snapmaker 2.0 (A150, A250, A350). Monitoring only.",
        address: Address::Host,
        slicing: Slicing::None,
        model: "Snapmaker 2.0",
    },
    Kind {
        name: "snapmaker-j1",
        about: "Snapmaker J1 and Artisan. Monitoring only.",
        address: Address::Host,
        slicing: Slicing::None,
        model: "Snapmaker J1",
    },
];

/// Asks questions; `init` reads every answer that was not given as a flag through this.
pub trait Ask {
    /// Shows text on its own line.
    fn say(&mut self, text: &str) -> Result<(), String>;
    /// Returns one trimmed answer; the end of input is an error.
    fn line(&mut self, prompt: &str) -> Result<String, String>;
    /// Like `line`, without showing what is typed where the terminal allows it.
    fn secret(&mut self, prompt: &str) -> Result<String, String>;
}

/// Questions on standard error, answers from standard input.
pub struct Terminal {
    noted: bool,
}

impl Terminal {
    pub fn new() -> Self {
        Self { noted: false }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Ask for Terminal {
    fn say(&mut self, text: &str) -> Result<(), String> {
        writeln!(std::io::stderr(), "{text}").map_err(|_| "Cannot show the question.".into())
    }

    fn line(&mut self, prompt: &str) -> Result<String, String> {
        let mut err = std::io::stderr();
        write!(err, "{prompt}")
            .and_then(|()| err.flush())
            .map_err(|_| "Cannot show the question.")?;
        let mut answer = String::new();
        std::io::stdin()
            .lock()
            .take(MAX_ANSWER + 1)
            .read_line(&mut answer)
            .map_err(|_| "Cannot read your answer.")?;
        finish_answer(&answer)
    }

    fn secret(&mut self, prompt: &str) -> Result<String, String> {
        #[cfg(unix)]
        if let Some(quiet) = EchoOff::new() {
            let answer = self.line(prompt);
            drop(quiet);
            return answer;
        }
        if !self.noted {
            self.noted = true;
            self.say("This terminal shows what you type, including secrets.")?;
        }
        self.line(prompt)
    }
}

/// Hides typed characters until dropped, and restores them if Ctrl-C ends the program first.
#[cfg(unix)]
struct EchoOff(rustix::termios::Termios);

#[cfg(unix)]
fn restore(saved: &rustix::termios::Termios) {
    let _ = rustix::termios::tcsetattr(
        std::io::stdin(),
        rustix::termios::OptionalActions::Now,
        saved,
    );
}

#[cfg(unix)]
impl EchoOff {
    fn new() -> Option<Self> {
        use rustix::termios::{tcgetattr, tcsetattr, LocalModes, OptionalActions};
        let stdin = std::io::stdin();
        let saved = tcgetattr(&stdin).ok()?;
        let mut quiet = saved.clone();
        quiet.local_modes.remove(LocalModes::ECHO);
        quiet.local_modes.insert(LocalModes::ECHONL);
        // Listening for Ctrl-C replaces its default action for the rest of the run, so the
        // listener stays until exit; restoring the saved settings again is harmless.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let saved = saved.clone();
            runtime.spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    restore(&saved);
                    eprintln!();
                    eprintln!("Stopped. No file was written.");
                    std::process::exit(130);
                }
            });
        }
        tcsetattr(&stdin, OptionalActions::Now, &quiet).ok()?;
        Some(Self(saved))
    }
}

#[cfg(unix)]
impl Drop for EchoOff {
    fn drop(&mut self) {
        restore(&self.0);
    }
}

fn finish_answer(answer: &str) -> Result<String, String> {
    if answer.is_empty() {
        return Err("Stopped before every question was answered. No file was written.".into());
    }
    if answer.len() as u64 > MAX_ANSWER {
        return Err("That answer is too long. No file was written.".into());
    }
    Ok(answer.trim().to_owned())
}

/// Runs `init`: asks on a terminal, otherwise takes flags only, then writes the file.
pub fn run(flags: Flags, config: Option<PathBuf>) -> Result<(), String> {
    use std::io::IsTerminal as _;
    let path = match flags.output.clone().or(config) {
        Some(path) => path,
        None => crate::config::default_path()?,
    };
    if !flags.force && std::fs::symlink_metadata(&path).is_ok() {
        return Err(format!(
            "{} already exists. Use --force to replace it.",
            path.display()
        ));
    }
    let mut terminal = Terminal::new();
    let ask: Option<&mut dyn Ask> = if std::io::stdin().is_terminal() {
        Some(&mut terminal)
    } else {
        None
    };
    let setup = build(&flags, ask)?;
    write(&path, &setup.text, flags.force)?;
    let config = if crate::config::default_path().ok().as_deref() == Some(path.as_path()) {
        String::new()
    } else {
        format!(" --config {}", shell_path(&path))
    };
    println!(
        "Wrote {} for {} ({}).",
        path.display(),
        setup.name,
        setup.kind
    );
    if setup.kind == "simulated" {
        println!("The simulated printer contacts nothing.");
    } else {
        println!("Nothing has been sent to the printer yet; init only wrote this file.");
    }
    if setup.needs_trust {
        println!(
            "Next, read the printer's certificate, check it, and set cert_sha256 in the file:"
        );
        println!("  dutchman-print{config} fingerprint {}", setup.id);
        println!("Then check that Print can read the printer:");
    } else {
        println!("Next, check that Print can read the printer:");
    }
    println!("  dutchman-print{config} printers");
    println!("Then start the job API for the SolidCraft plugin:");
    println!("  dutchman-print{config} serve");
    Ok(())
}

fn shell_path(path: &Path) -> String {
    let text = path.display().to_string();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/\\:._-~".contains(c))
    {
        text
    } else {
        format!("\"{text}\"")
    }
}

/// The checked config text and what the closing message needs; never secrets.
pub struct Setup {
    pub text: String,
    pub kind: &'static str,
    pub id: String,
    pub name: String,
    /// A Bambu printer written without cert_sha256 or ca_file.
    pub needs_trust: bool,
}

#[derive(Default)]
struct Answers {
    url: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    api_key: Option<String>,
    username: Option<String>,
    password: Option<String>,
    serial: Option<String>,
    access_code: Option<String>,
    cert_sha256: Option<String>,
    ca_file: Option<String>,
    profile: Option<String>,
    nozzle_diameter: Option<f64>,
    slug: Option<String>,
    slicer: Option<&'static str>,
    profiles: Option<[String; 3]>,
    id: String,
    name: String,
    model: String,
}

struct Session<'a> {
    ask: Option<&'a mut dyn Ask>,
}

fn missing(flag: &str) -> String {
    format!("Missing --{flag}. Without a terminal, give every answer as a flag.")
}

impl Session<'_> {
    fn say(&mut self, text: &str) -> Result<(), String> {
        match self.ask.as_deref_mut() {
            Some(ask) => ask.say(text),
            None => Ok(()),
        }
    }

    fn required(
        &mut self,
        given: Option<&String>,
        flag: &str,
        prompt: &str,
        default: Option<&str>,
    ) -> Result<String, String> {
        if let Some(value) = given {
            return Ok(value.clone());
        }
        let Some(ask) = self.ask.as_deref_mut() else {
            return default.map(str::to_owned).ok_or_else(|| missing(flag));
        };
        let shown = match default {
            Some(default) => format!("{prompt} [{default}]: "),
            None => format!("{prompt}: "),
        };
        loop {
            let answer = ask.line(&shown)?;
            if !answer.is_empty() {
                return Ok(answer);
            }
            if let Some(default) = default {
                return Ok(default.to_owned());
            }
        }
    }

    fn optional(&mut self, given: Option<&String>, prompt: &str) -> Result<Option<String>, String> {
        if let Some(value) = given {
            return Ok(Some(value.clone()));
        }
        let Some(ask) = self.ask.as_deref_mut() else {
            return Ok(None);
        };
        let answer = ask.line(&format!("{prompt}: "))?;
        Ok(Some(answer).filter(|a| !a.is_empty()))
    }

    fn secret(
        &mut self,
        given: Option<&String>,
        flag: &str,
        prompt: &str,
        required: bool,
    ) -> Result<Option<String>, String> {
        if let Some(value) = given {
            return Ok(Some(value.clone()));
        }
        let Some(ask) = self.ask.as_deref_mut() else {
            return if required {
                Err(missing(flag))
            } else {
                Ok(None)
            };
        };
        loop {
            let answer = ask.secret(&format!("{prompt}: "))?;
            if !answer.is_empty() || !required {
                return Ok(Some(answer).filter(|a| !a.is_empty()));
            }
        }
    }

    /// A numbered choice; answers are a number or one of the names.
    fn choose(
        &mut self,
        prompt: &str,
        names: &[&'static str],
        default: Option<usize>,
    ) -> Result<&'static str, String> {
        let Some(ask) = self.ask.as_deref_mut() else {
            return Err("Cannot ask without a terminal.".into());
        };
        let shown = match default.and_then(|d| names.get(d)) {
            Some(name) => format!("{prompt} [{name}]: "),
            None => format!("{prompt}: "),
        };
        loop {
            let answer = ask.line(&shown)?.to_ascii_lowercase();
            let picked = if answer.is_empty() {
                default
            } else {
                answer
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .or_else(|| names.iter().position(|n| *n == answer))
            };
            if let Some(name) = picked.and_then(|i| names.get(i)) {
                return Ok(name);
            }
            ask.say(&format!(
                "Choose a number from 1 to {}, or type the name.",
                names.len()
            ))?;
        }
    }
}

fn kind_by_name(name: &str) -> Result<&'static Kind, String> {
    KINDS.iter().find(|k| k.name == name).ok_or_else(|| {
        let names: Vec<_> = KINDS.iter().map(|k| k.name).collect();
        format!("Set --kind to one of: {}.", names.join(", "))
    })
}

/// Refuses flags that the chosen kind would not use, so a script finds its mistake.
fn check_flags(flags: &Flags, kind: &Kind) -> Result<(), String> {
    let name = kind.name;
    if flags.url.is_some() && kind.address != Address::Url {
        return Err(match kind.address {
            Address::Host => format!("Use --host, not --url, for {name} printers."),
            _ => format!("--url does not apply to {name} printers."),
        });
    }
    if flags.host.is_some() && kind.address != Address::Host {
        return Err(match kind.address {
            Address::Url => format!("Use --url, not --host, for {name} printers."),
            _ => format!("--host does not apply to {name} printers."),
        });
    }
    let slices = kind.slicing != Slicing::None;
    let unused = [
        (
            "port",
            flags.port.is_some() && kind.address != Address::Host,
        ),
        (
            "api-key",
            flags.api_key.is_some()
                && !matches!(
                    name,
                    "prusalink" | "octoprint" | "moonraker" | "snapmaker-u1" | "repetier"
                ),
        ),
        ("username", flags.username.is_some() && name != "prusalink"),
        (
            "password",
            flags.password.is_some() && !matches!(name, "prusalink" | "duet"),
        ),
        ("serial", flags.serial.is_some() && name != "bambu"),
        (
            "access-code",
            flags.access_code.is_some() && name != "bambu",
        ),
        (
            "cert-sha256",
            flags.cert_sha256.is_some() && name != "bambu",
        ),
        ("ca-file", flags.ca_file.is_some() && name != "bambu"),
        ("profile", flags.profile.is_some() && name != "elegoo-sdcp"),
        (
            "nozzle-diameter",
            flags.nozzle_diameter.is_some() && !matches!(name, "duet" | "repetier"),
        ),
        ("slug", flags.slug.is_some() && name != "repetier"),
        ("slicer", flags.slicer.is_some() && !slices),
        (
            "printer-profile",
            flags.printer_profile.is_some() && !slices,
        ),
        ("print-profile", flags.print_profile.is_some() && !slices),
        (
            "filament-profile",
            flags.filament_profile.is_some() && !slices,
        ),
    ];
    if let Some((flag, _)) = unused.iter().find(|(_, unused)| *unused) {
        return Err(format!("--{flag} does not apply to {name} printers."));
    }
    if name == "prusalink" && flags.api_key.is_some() && flags.username.is_some() {
        return Err("Use either --api-key or --username and --password, not both.".into());
    }
    if flags.cert_sha256.is_some() && flags.ca_file.is_some() {
        return Err("Use either --cert-sha256 or --ca-file, not both.".into());
    }
    Ok(())
}

/// Gathers answers from flags and, when `ask` is given, questions; returns checked config text.
pub fn build(flags: &Flags, ask: Option<&mut dyn Ask>) -> Result<Setup, String> {
    let mut session = Session { ask };
    let kind = match &flags.kind {
        Some(name) => kind_by_name(&name.to_ascii_lowercase())?,
        None if session.ask.is_none() => return Err(missing("kind")),
        None => {
            session.say("Printer types:")?;
            for (i, kind) in KINDS.iter().enumerate() {
                session.say(&format!("{:>3}. {:<13} {}", i + 1, kind.name, kind.about))?;
            }
            let names: Vec<_> = KINDS.iter().map(|k| k.name).collect();
            kind_by_name(session.choose("Printer type (number or name)", &names, None)?)?
        }
    };
    check_flags(flags, kind)?;
    let mut answers = Answers {
        port: flags.port,
        ..Answers::default()
    };
    if kind.name == "elegoo-sdcp" {
        answers.profile = Some(match &flags.profile {
            Some(profile) if matches!(profile.as_str(), "centauri" | "resin") => profile.clone(),
            Some(_) => return Err("Set --profile to centauri or resin.".into()),
            None if session.ask.is_none() => return Err(missing("profile")),
            None => {
                session.say("  1. centauri  Elegoo Centauri Carbon")?;
                session.say("  2. resin     Elegoo resin printers")?;
                session
                    .choose(
                        "Elegoo printer (number or name)",
                        &["centauri", "resin"],
                        None,
                    )?
                    .to_owned()
            }
        });
    }
    match kind.address {
        Address::Url => {
            let url = session.required(
                flags.url.as_ref(),
                "url",
                "Printer address, such as http://192.168.1.30",
                None,
            )?;
            answers.url = Some(if url.contains("://") {
                url
            } else {
                format!("http://{url}")
            });
        }
        Address::Host => {
            answers.host = Some(session.required(
                flags.host.as_ref(),
                "host",
                "Printer IP address or hostname, such as 192.168.1.30",
                None,
            )?);
        }
        Address::None => {}
    }
    secrets(flags, kind, &mut session, &mut answers)?;
    if kind.name == "repetier" {
        answers.slug = Some(session.required(
            flags.slug.as_ref(),
            "slug",
            "Repetier-Server printer slug, shown in the server's printer settings",
            None,
        )?);
    }
    if matches!(kind.name, "duet" | "repetier") {
        answers.nozzle_diameter = Some(nozzle(flags, &mut session)?);
    }
    let default_name = if kind.name == "simulated" {
        "Workbench demo"
    } else {
        kind.model
    };
    answers.name = session.required(
        flags.name.as_ref(),
        "name",
        "Printer name, shown in lists",
        Some(default_name),
    )?;
    answers.id = session.required(
        flags.id.as_ref(),
        "id",
        "Printer id, used in commands",
        Some(&slug(&answers.name)),
    )?;
    answers.model = session.required(
        flags.model.as_ref(),
        "model",
        "Printer model",
        Some(kind.model),
    )?;
    if kind.slicing != Slicing::None {
        slicing(flags, kind, &mut session, &mut answers)?;
    }
    let text = render(kind, &answers);
    let needs_trust =
        kind.name == "bambu" && answers.cert_sha256.is_none() && answers.ca_file.is_none();
    let enrollment = needs_trust.then_some(answers.id.as_str());
    crate::config::Config::from_text(&text, enrollment)
        .map_err(|err| format!("{err} No file was written."))?;
    Ok(Setup {
        text,
        kind: kind.name,
        id: answers.id,
        name: answers.name,
        needs_trust,
    })
}

fn secrets(
    flags: &Flags,
    kind: &Kind,
    session: &mut Session<'_>,
    answers: &mut Answers,
) -> Result<(), String> {
    match kind.name {
        "prusalink" => {
            if flags.username.is_some() || flags.password.is_some() {
                answers.username = Some(session.required(
                    flags.username.as_ref(),
                    "username",
                    "PrusaLink username",
                    None,
                )?);
                answers.password =
                    session.secret(flags.password.as_ref(), "password", "Password", true)?;
                return Ok(());
            }
            if session.ask.is_none() && flags.api_key.is_none() {
                return Err(
                    "Missing --api-key (or --username and --password). Without a terminal, give every answer as a flag."
                        .into(),
                );
            }
            session
                .say("Find the API key on the printer. On MINI: Settings > Network > PrusaLink.")?;
            answers.api_key = session.secret(
                flags.api_key.as_ref(),
                "api-key",
                "PrusaLink API key (leave empty to use a username and password)",
                false,
            )?;
            if answers.api_key.is_none() {
                answers.username =
                    Some(session.required(None, "username", "PrusaLink username", None)?);
                answers.password = session.secret(None, "password", "Password", true)?;
            }
        }
        "octoprint" => {
            session.say("Create an API key in OctoPrint under Settings > Application Keys.")?;
            answers.api_key =
                session.secret(flags.api_key.as_ref(), "api-key", "OctoPrint API key", true)?;
        }
        "moonraker" | "snapmaker-u1" => {
            answers.api_key = session.secret(
                flags.api_key.as_ref(),
                "api-key",
                "Moonraker API key (leave empty if Moonraker trusts this computer)",
                false,
            )?;
        }
        "repetier" => {
            answers.api_key = session.secret(
                flags.api_key.as_ref(),
                "api-key",
                "Repetier-Server API key",
                true,
            )?;
        }
        "duet" => {
            answers.password = session.secret(
                flags.password.as_ref(),
                "password",
                "Machine password set with M551 (leave empty if none is set)",
                false,
            )?;
        }
        "bambu" => {
            session.say("Turn on LAN mode on the printer first. The serial is on its label or device information screen.")?;
            answers.serial = Some(session.required(
                flags.serial.as_ref(),
                "serial",
                "Printer serial number",
                None,
            )?);
            answers.access_code = session.secret(
                flags.access_code.as_ref(),
                "access-code",
                "LAN access code, from the printer's network settings",
                true,
            )?;
            answers.ca_file = match &flags.ca_file {
                Some(path) => Some(
                    path.to_str()
                        .ok_or("Use a --ca-file path made of plain text.")?
                        .to_owned(),
                ),
                None => None,
            };
            if answers.ca_file.is_none() {
                answers.cert_sha256 = session.optional(
                    flags.cert_sha256.as_ref(),
                    "Certificate SHA-256 fingerprint (leave empty to read it later with fingerprint)",
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn nozzle(flags: &Flags, session: &mut Session<'_>) -> Result<f64, String> {
    const RANGE: &str = "Set the nozzle diameter in millimetres, greater than 0 and at most 10.";
    let fits = |d: f64| d.is_finite() && d > 0.0 && d <= 10.0;
    if let Some(diameter) = flags.nozzle_diameter {
        return if fits(diameter) {
            Ok(diameter)
        } else {
            Err(RANGE.into())
        };
    }
    loop {
        let answer = session.required(
            None,
            "nozzle-diameter",
            "Installed nozzle diameter in millimetres, such as 0.4",
            None,
        )?;
        match answer.parse::<f64>() {
            Ok(diameter) if fits(diameter) => return Ok(diameter),
            _ => session.say(RANGE)?,
        }
    }
}

fn slicing(
    flags: &Flags,
    kind: &Kind,
    session: &mut Session<'_>,
    answers: &mut Answers,
) -> Result<(), String> {
    let fixed = match kind.slicing {
        Slicing::Prusa => Some("prusaslicer"),
        Slicing::Orca => Some("orcaslicer"),
        _ => None,
    };
    let slicer = match (fixed, flags.slicer.as_deref().map(str::to_ascii_lowercase)) {
        (_, Some(s)) if !matches!(s.as_str(), "prusaslicer" | "orcaslicer") => {
            return Err("Set --slicer to prusaslicer or orcaslicer.".into())
        }
        (Some(fixed), Some(s)) if s != fixed => {
            return Err(format!(
                "{} printers need {fixed}. Leave out --slicer or set it to {fixed}.",
                kind.name
            ))
        }
        (Some(fixed), _) => {
            session.say(&format!(
                "This printer slices with {}.",
                slicer_title(fixed)
            ))?;
            fixed
        }
        (None, Some(s)) if s == "orcaslicer" => "orcaslicer",
        (None, Some(_)) => "prusaslicer",
        (None, None) if session.ask.is_none() => "prusaslicer",
        (None, None) => {
            session.say("  1. prusaslicer  PrusaSlicer")?;
            session.say("  2. orcaslicer   OrcaSlicer")?;
            session.choose(
                "Installed slicer (number or name)",
                &["prusaslicer", "orcaslicer"],
                Some(0),
            )?
        }
    };
    answers.slicer = Some(slicer);
    let defaults = profile_defaults(slicer);
    session.say(&format!(
        "Enter the exact preset names from {}. {}",
        slicer_title(slicer),
        profile_place(slicer)
    ))?;
    let [printer, print, filament] = defaults;
    answers.profiles = Some([
        session.required(
            flags.printer_profile.as_ref(),
            "printer-profile",
            "Printer preset",
            Some(printer),
        )?,
        session.required(
            flags.print_profile.as_ref(),
            "print-profile",
            "Print preset",
            Some(print),
        )?,
        session.required(
            flags.filament_profile.as_ref(),
            "filament-profile",
            "Filament preset",
            Some(filament),
        )?,
    ]);
    Ok(())
}

fn slicer_title(kind: &str) -> &'static str {
    if kind == "orcaslicer" {
        "OrcaSlicer"
    } else {
        "PrusaSlicer"
    }
}

fn profile_place(kind: &str) -> &'static str {
    if kind == "orcaslicer" {
        "They are the names in its printer, process, and filament preset lists."
    } else {
        "They are the names in the Printer, Print settings, and Filament lists on its Plater tab."
    }
}

fn profile_defaults(kind: &str) -> [&'static str; 3] {
    if kind == "orcaslicer" {
        [
            "Bambu Lab A1 0.4 nozzle",
            "0.20mm Standard @BBL A1",
            "Generic PLA @BBL A1",
        ]
    } else {
        [
            "Original Prusa MINI & MINI+",
            "0.20mm QUALITY @MINI",
            "Generic PLA @MINI",
        ]
    }
}

/// An id from a name: lower-case letters and digits joined by hyphens.
fn slug(name: &str) -> String {
    let mut id = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
        if id.len() >= 64 {
            break;
        }
    }
    let id = id.trim_end_matches('-');
    if id.is_empty() {
        "printer".into()
    } else {
        id.into()
    }
}

/// A TOML basic string.
fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct Text(String);

impl Text {
    fn comment(&mut self, lines: &str) {
        for line in lines.lines() {
            self.0.push_str("# ");
            self.0.push_str(line);
            self.0.push('\n');
        }
    }
    fn line(&mut self, line: &str) {
        self.0.push_str(line);
        self.0.push('\n');
    }
    fn string(&mut self, key: &str, value: &str) {
        self.line(&format!("{key} = {}", quote(value)));
    }
    fn maybe(&mut self, key: &str, value: Option<&String>, example: &str) {
        match value {
            Some(value) => self.string(key, value),
            None => self.line(&format!("# {key} = {}", quote(example))),
        }
    }
}

fn render(kind: &Kind, a: &Answers) -> String {
    let mut t = Text(String::new());
    t.comment(
        "Dutchman Print config, written by dutchman-print init.\n\
         Keep this file private: it can hold printer passwords and keys.\n\
         docs/print.md describes every field. Repeat [[printers]] to add more printers.",
    );
    t.line("");
    t.line("[backend]");
    if kind.name == "simulated" {
        t.comment("\"simulated\" is a built-in demo: it contacts no printer and needs no slicer.");
        t.string("kind", "simulated");
    } else {
        t.comment("\"printers\" talks to the real printers listed below.");
        t.string("kind", "printers");
    }
    if let Some(slicer) = a.slicer {
        let title = slicer_title(slicer);
        let program = if slicer == "orcaslicer" {
            "orca-slicer"
        } else {
            "prusa-slicer"
        };
        t.line("");
        t.comment(&format!(
            "The slicer that prepares jobs. Install {title} separately and set up\n\
             your printer in it first."
        ));
        t.line("[slicer]");
        t.string("kind", slicer);
        t.comment(
            "Print looks for the slicer on PATH and in the usual install folders. Set path\n\
             to the executable when it is somewhere else, such as an AppImage.",
        );
        t.line(&format!(
            "# path = {}",
            quote(&format!("/path/to/{program}"))
        ));
        t.comment("Set datadir only to use a slicer data folder other than its usual one.");
        t.line(&format!(
            "# datadir = {}",
            quote(&format!("/path/to/{title}"))
        ));
        t.comment("Longest time one slice may take, in seconds (1 to 86400).");
        t.line("timeout_seconds = 600");
    }
    t.line("");
    t.line("[[printers]]");
    if kind.name != "simulated" {
        t.comment(kind.about);
        t.string("kind", kind.name);
    }
    t.comment("Short id for commands, such as dutchman-print pause <id>. Use letters,\ndigits, hyphens, and underscores.");
    t.string("id", &a.id);
    t.comment("Name shown in printer lists and in SolidCraft.");
    t.string("name", &a.name);
    t.comment("Printer model, as free text.");
    t.string("model", &a.model);
    printer_fields(&mut t, kind, a);
    if let (Some(slicer), Some([printer, print, filament])) = (a.slicer, &a.profiles) {
        t.comment(&format!(
            "Slicer presets, by their exact names in {}.\n{}\n\
             System presets and presets you saved both work. Change these examples to the\n\
             presets set up for this printer's model, nozzle, material, and plate.",
            slicer_title(slicer),
            profile_place(slicer)
        ));
        t.string("printer_profile", printer);
        t.string("print_profile", print);
        t.string("filament_profile", filament);
    }
    t.0
}

fn printer_fields(t: &mut Text, kind: &Kind, a: &Answers) {
    let url = a.url.as_deref().unwrap_or_default();
    let host = a.host.as_deref().unwrap_or_default();
    match kind.name {
        "prusalink" => {
            t.comment(
                "The printer's local address as http://host or http://host:port, without a path.\n\
                 Find its IP address in the printer's network settings.",
            );
            t.string("url", url);
            if a.api_key.is_some() {
                t.comment(
                    "PrusaLink API key. On MINI: Settings > Network > PrusaLink > Current API Key.\n\
                     Pi-hosted PrusaLink 0.7.0 and later shows it in Prusa Connect's Settings.",
                );
                t.maybe("api_key", a.api_key.as_ref(), "");
            } else {
                t.comment(
                    "PrusaLink login, sent with HTTP Digest. MK4 and XL use the username \"maker\"\n\
                     and the API key as the password.",
                );
                t.maybe("username", a.username.as_ref(), "maker");
                t.maybe("password", a.password.as_ref(), "");
            }
        }
        "octoprint" => {
            t.comment("OctoPrint's address: http or https, with its base path if it has one.");
            t.string("url", url);
            t.comment(
                "OctoPrint API key: an application key from Settings > Application Keys, or the\n\
                 personal key in your user settings. It needs status, upload, and print permissions.",
            );
            t.maybe("api_key", a.api_key.as_ref(), "");
        }
        "moonraker" | "snapmaker-u1" => {
            t.comment(
                "Moonraker's address, such as http://printer.local:7125, with its base path when\n\
                 it sits behind a proxy.",
            );
            t.string("url", url);
            t.comment(
                "Moonraker API key, from your interface's authorization settings. Leave it out\n\
                 only when Moonraker trusts this computer.",
            );
            t.maybe("api_key", a.api_key.as_ref(), "YOUR_MOONRAKER_KEY");
            t.comment("Set true only after checking that this printer's pause and resume macros\nbehave safely.");
            t.line("resume_commissioned = false");
        }
        "repetier" => {
            t.comment("Repetier-Server's address, such as http://repetier.local:3344.");
            t.string("url", url);
            t.comment(
                "API key from Repetier-Server's user settings, with print, add and delete rights.\n\
                 It is sent only in a request header.",
            );
            t.maybe("api_key", a.api_key.as_ref(), "YOUR_REPETIER_KEY");
            t.comment("The printer's slug, shown in the server's printer settings.");
            t.maybe("slug", a.slug.as_ref(), "");
            t.comment(
                "Installed nozzle diameter in millimetres. The server does not report it, so check\n\
                 the nozzle itself and change this when you change the nozzle.",
            );
            t.line(&format!(
                "nozzle_diameter = {:?}",
                a.nozzle_diameter.unwrap_or_default()
            ));
            t.comment(
                "Turn off \"Autostart next job\" in the server's printer settings: Stop cannot name\n\
                 a job, so a job the server starts by itself at that moment would be stopped.",
            );
        }
        "duet" => {
            t.comment("The Duet board's address, such as http://duet.local.");
            t.string("url", url);
            t.comment("Machine password set with M551. Leave it out when none is set.");
            t.maybe("password", a.password.as_ref(), "YOUR_MACHINE_PASSWORD");
            t.comment(
                "Installed nozzle diameter in millimetres. The board does not report it, so check\n\
                 the nozzle itself and change this when you change the nozzle.",
            );
            t.line(&format!(
                "nozzle_diameter = {:?}",
                a.nozzle_diameter.unwrap_or_default()
            ));
        }
        "bambu" => {
            t.comment(
                "The printer's IP address or hostname, without a port. Turn on LAN mode first;\n\
                 newer firmware may also need Developer Mode.",
            );
            t.string("host", host);
            port_line(t, a, "8883", "Optional MQTT TLS port; defaults to 8883.");
            t.line("# ftps_port = 990 # Optional FTPS port for uploads; defaults to 990.");
            t.comment("Serial number from the printer's label or its device information screen.");
            t.maybe("serial", a.serial.as_ref(), "");
            t.comment("LAN access code from the printer's LAN or network settings.");
            t.maybe("access_code", a.access_code.as_ref(), "");
            t.comment(
                "Trust: set exactly one of cert_sha256 or ca_file. cert_sha256 is the SHA-256 of\n\
                 the printer's certificate, 64 hexadecimal digits. Read it with\n\
                 dutchman-print fingerprint <id>, then check it independently.",
            );
            t.maybe(
                "cert_sha256",
                a.cert_sha256.as_ref(),
                "64 hexadecimal digits",
            );
            t.maybe(
                "ca_file",
                a.ca_file.as_ref(),
                "/path/to/trusted-bambu-ca.pem",
            );
            t.comment(
                "Control is experimental and checked against simulated printers only. Leave this\n\
                 false to monitor; set it true only after confirming LAN or Developer Mode\n\
                 allows control.",
            );
            t.line("lan_control = false");
        }
        "elegoo-sdcp" => {
            t.comment(
                "Resin printers also accept Stop. Print never slices or sends jobs to this\n\
                 printer, so it needs no slicer.",
            );
            t.comment("\"centauri\" for the Centauri Carbon, \"resin\" for resin printers.");
            t.maybe("profile", a.profile.as_ref(), "centauri");
            t.comment("The printer's IP address or hostname, without a scheme or path.");
            t.string("host", host);
            port_line(
                t,
                a,
                "3030",
                "Optional; resin defaults to 3030, Centauri to 80.",
            );
            t.line("# mainboard_id = \"000000000001d354\" # Optional; refuse reports from any other mainboard.");
        }
        "snapmaker-2" => {
            t.comment(
                "Print never slices or sends jobs to this printer. The first status read asks\n\
                 you to approve the connection on the touchscreen.",
            );
            t.comment("The printer's IP address or hostname, without a scheme or path.");
            t.string("host", host);
            port_line(t, a, "8080", "Optional; defaults to 8080.");
        }
        "snapmaker-j1" => {
            t.comment(
                "Print reads installed modules and firmware, never print state. It never slices\n\
                 or sends jobs to this printer.",
            );
            t.comment("The printer's IP address or hostname, without a scheme or path.");
            t.string("host", host);
            port_line(t, a, "8888", "Optional; defaults to 8888.");
        }
        _ => {}
    }
}

fn port_line(t: &mut Text, a: &Answers, default: &str, note: &str) {
    match a.port {
        Some(port) => t.line(&format!("port = {port} # {note}")),
        None => t.line(&format!("# port = {default} # {note}")),
    }
}

/// Writes `text` privately: a new file, or with `force` a replacement put in place whole.
pub fn write(path: &Path, text: &str, force: bool) -> Result<(), String> {
    let shown = path.display();
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let mut folder = std::fs::DirBuilder::new();
        folder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut folder, 0o700);
        folder
            .create(parent)
            .map_err(|err| format!("Cannot create the folder for {shown}: {err}"))?;
    }
    if !force {
        return write_new(path, text).map_err(|err| match err.kind() {
            std::io::ErrorKind::AlreadyExists => {
                format!("{shown} already exists. Use --force to replace it.")
            }
            _ => format!("Cannot write {shown}: {err}"),
        });
    }
    if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        return Err(format!("{shown} is a folder. Choose a file with --output."));
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".init-{}", std::process::id()));
    let staged = path.with_file_name(name);
    let result = write_new(&staged, text).and_then(|()| std::fs::rename(&staged, path));
    if let Err(err) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("Cannot write {shown}: {err}"));
    }
    Ok(())
}

fn write_new(path: &Path, text: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    if written.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted answers; records which prompts were asked as secrets.
    struct Script {
        answers: std::collections::VecDeque<&'static str>,
        shown: String,
        secrets: Vec<String>,
    }

    impl Script {
        fn new(answers: &[&'static str]) -> Self {
            Self {
                answers: answers.iter().copied().collect(),
                shown: String::new(),
                secrets: Vec::new(),
            }
        }
    }

    impl Ask for Script {
        fn say(&mut self, text: &str) -> Result<(), String> {
            self.shown.push_str(text);
            self.shown.push('\n');
            Ok(())
        }
        fn line(&mut self, prompt: &str) -> Result<String, String> {
            self.shown.push_str(prompt);
            let answer = self.answers.pop_front().ok_or("end of input")?;
            Ok(answer.trim().to_owned())
        }
        fn secret(&mut self, prompt: &str) -> Result<String, String> {
            self.secrets.push(prompt.to_owned());
            self.line(prompt)
        }
    }

    #[test]
    fn interactive_prusalink_uses_defaults_and_hides_the_key() {
        let mut script = Script::new(&[
            "9000", // not a kind: asked again
            "2",
            "192.168.1.30",
            "secret-key",
            "Workbench MINI",
            "",
            "",
            "",
            "",
            "Prusament PLA",
        ]);
        let setup = build(&Flags::default(), Some(&mut script)).unwrap();
        assert_eq!(setup.kind, "prusalink");
        assert_eq!(setup.id, "workbench-mini");
        assert!(!setup.needs_trust);
        assert!(script.shown.contains("Choose a number from 1 to 11"));
        assert!(script.shown.contains("  7. repetier"));
        assert!(script.shown.contains("  8. bambu"));
        assert_eq!(script.secrets.len(), 1);
        assert!(script.secrets[0].contains("API key"));
        let config = crate::config::Config::from_text(&setup.text, None).unwrap();
        let printer = &config.printers[0];
        assert_eq!(printer.url.as_deref(), Some("http://192.168.1.30"));
        assert_eq!(printer.api_key.as_deref(), Some("secret-key"));
        assert_eq!(printer.model, "Prusa printer");
        assert_eq!(
            printer.printer_profile.as_deref(),
            Some("Original Prusa MINI & MINI+")
        );
        assert_eq!(printer.filament_profile.as_deref(), Some("Prusament PLA"));
        assert_eq!(config.slicer.unwrap().kind, "prusaslicer");
    }

    #[test]
    fn interactive_prusalink_falls_back_to_a_login() {
        let mut script = Script::new(&[
            "prusalink",
            "http://mk4.local",
            "",
            "maker",
            "",
            "pass\"word\\",
            "",
            "",
            "",
            "",
            "",
            "",
        ]);
        let setup = build(&Flags::default(), Some(&mut script)).unwrap();
        assert_eq!(script.secrets.len(), 3, "key, then the password twice");
        let config = crate::config::Config::from_text(&setup.text, None).unwrap();
        let printer = &config.printers[0];
        assert_eq!(printer.api_key, None);
        assert_eq!(printer.username.as_deref(), Some("maker"));
        assert_eq!(printer.password.as_deref(), Some("pass\"word\\"));
    }

    #[test]
    fn interactive_bambu_can_leave_trust_for_later() {
        let mut script = Script::new(&[
            "bambu",
            "192.168.1.40",
            "01S00A000000000",
            "12345678",
            "",
            "Bench A1",
            "a1",
            "Bambu Lab A1",
            "",
            "",
            "",
        ]);
        let setup = build(&Flags::default(), Some(&mut script)).unwrap();
        assert!(setup.needs_trust);
        assert!(script.shown.contains("slices with OrcaSlicer"));
        assert!(setup.text.contains("\nlan_control = false\n"));
        assert!(crate::config::Config::from_text(&setup.text, None).is_err());
        let config = crate::config::Config::from_text(&setup.text, Some("a1")).unwrap();
        assert!(!config.printers[0].lan_control);
    }

    #[test]
    fn interactive_duet_asks_again_for_a_usable_nozzle() {
        let mut script = Script::new(&[
            "duet",
            "duet.local",
            "",
            "abc",
            "0",
            "0.6",
            "",
            "",
            "",
            "2",
            "",
            "",
            "",
        ]);
        let setup = build(&Flags::default(), Some(&mut script)).unwrap();
        let config = crate::config::Config::from_text(&setup.text, None).unwrap();
        let printer = &config.printers[0];
        assert_eq!(printer.nozzle_diameter, Some(0.6));
        assert_eq!(printer.password, None);
        assert_eq!(config.slicer.unwrap().kind, "orcaslicer");
        assert!(setup.text.contains("# password = "));
    }

    #[test]
    fn interactive_elegoo_and_end_of_input() {
        let mut script = Script::new(&["9", "2", "192.168.1.41", "", "", ""]);
        let setup = build(&Flags::default(), Some(&mut script)).unwrap();
        let config = crate::config::Config::from_text(&setup.text, None).unwrap();
        assert_eq!(config.printers[0].profile.as_deref(), Some("resin"));
        assert!(config.slicer.is_none());

        let mut short = Script::new(&["octoprint", "http://octopi.local"]);
        assert!(build(&Flags::default(), Some(&mut short)).is_err());
    }

    #[test]
    fn values_are_quoted_and_ids_are_derived() {
        assert_eq!(quote("a\"b\\c\u{7f}"), "\"a\\\"b\\\\c\\u007F\"");
        assert_eq!(slug("  Bench MINI+ #2 "), "bench-mini-2");
        assert_eq!(slug("!!!"), "printer");
        assert!(slug(&"x".repeat(100)).len() <= 64);
    }
}
