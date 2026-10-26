//! Command line for the printer companion.

use std::io::IsTerminal as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::{Parser, Subcommand};
use dutchman_print::backend::{Backend, Options};
use dutchman_print::simulated::{Clock, Simulated, SystemClock};
use dutchman_print::{config, ServerConfig};

#[derive(Parser)]
#[command(
    name = "dutchman-print",
    version,
    about = "Prepare and control print jobs for SolidCraft"
)]
struct Args {
    /// Private printer configuration.
    #[arg(long, env = "DUTCHMAN_PRINT_CONFIG", global = true)]
    config: Option<PathBuf>,
    /// Private job storage (default: dutchman-print in the temporary folder).
    #[arg(long, env = "DUTCHMAN_PRINT_DATA", global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// List printers.
    Printers {
        #[arg(long)]
        json: bool,
    },
    /// Prepare a 3MF and ask before starting it.
    Print {
        file: PathBuf,
        #[arg(long)]
        printer: String,
        #[arg(long)]
        printer_profile: Option<String>,
        #[arg(long)]
        print_profile: Option<String>,
        #[arg(long)]
        filament_profile: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show a simulated CLI job.
    Status { job: String },
    /// Cancel a job after confirmation.
    Cancel {
        job: String,
        #[arg(long)]
        yes: bool,
    },
    /// Acknowledge that a printer bed has been checked and cleared.
    Clear {
        printer: String,
        #[arg(long)]
        yes: bool,
    },
    Pause {
        printer: String,
        #[arg(long)]
        yes: bool,
    },
    Resume {
        printer: String,
        #[arg(long)]
        yes: bool,
    },
    /// Run the plugin job API.
    Serve {
        #[arg(long, env = "DUTCHMAN_PRINT_LISTEN", default_value = "127.0.0.1:8730")]
        listen: SocketAddr,
        #[arg(long, env = "DUTCHMAN_PRINT_USER", default_value = "dutchman")]
        user: String,
        #[arg(long, env = "DUTCHMAN_PRINT_PUBLIC_ORIGIN", value_delimiter = ',')]
        public_origin: Vec<String>,
    },
}
struct WallClock;
impl Clock for WallClock {
    fn now(&self) -> Duration {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
    }
}
struct Lock(PathBuf);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("dutchman-print: {err}");
            ExitCode::FAILURE
        }
    }
}
async fn run(args: Args) -> Result<(), String> {
    let path = args.config.map(Ok).unwrap_or_else(config::default_path)?;
    let config = config::Config::load(&path)?;
    let data_dir = args
        .data_dir
        .unwrap_or_else(|| std::env::temp_dir().join("dutchman-print"));
    if config.backend.kind == "printers" {
        return real_commands(args.command, config, &data_dir, &path).await;
    }
    if let Command::Printers { json } = args.command {
        let backend = Simulated::new(config, Arc::new(WallClock))?;
        // Listing also reflects jobs prepared by earlier CLI commands when storage exists.
        let state = state_path(&data_dir, &path)?;
        if data_dir.exists() {
            dutchman_serve::prepare_data_dir(&data_dir)
                .map_err(|_| "Cannot use the private data folder.")?;
        }
        backend.restore(&state)?;
        let safety = dutchman_print::safety::Safety::new(Arc::new(backend));
        safety.restore(&state.with_extension("safety.json")).await?;
        let reports = safety.printers().await.map_err(|e| e.to_string())?;
        if json {
            output(&reports)?;
        } else {
            print!("{}", dutchman_print::text_report(&reports));
        }
        return Ok(());
    }
    dutchman_serve::prepare_data_dir(&data_dir)
        .map_err(|_| "Cannot use the private data folder.")?;
    if let Command::Serve {
        listen,
        user,
        public_origin,
    } = args.command
    {
        let backend = Arc::new(Simulated::new(config, Arc::new(SystemClock::default()))?);
        let login = std::env::var("DUTCHMAN_PRINT_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|password| (user, password));
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|err| format!("Cannot listen on {listen}: {err}"))?;
        if login.is_none() && !listen.ip().is_loopback() {
            eprintln!("dutchman-print: anyone who can reach {listen} can control jobs; set DUTCHMAN_PRINT_PASSWORD to ask for a password");
        }
        let address = listener.local_addr().map_err(|err| err.to_string())?;
        let mut settings = ServerConfig {
            login,
            public_origin,
            ..Default::default()
        };
        settings.storage.data_dir = data_dir.clone();
        let cleanup = tokio::spawn(dutchman_print::remove_expired(
            data_dir,
            settings.storage.keep,
        ));
        eprintln!("dutchman-print: listening on http://{address}");
        let result = axum::serve(listener, dutchman_print::router(backend, settings))
            .with_graceful_shutdown(stopped())
            .await;
        cleanup.abort();
        return result.map_err(|err| err.to_string());
    }
    let state = state_path(&data_dir, &path)?;
    let lock = state.with_extension("lock");
    std::fs::create_dir(&lock).map_err(|_| {
        format!(
            "Another CLI command is using simulated jobs. If none is running, remove {}.",
            lock.display()
        )
    })?;
    let _lock = Lock(lock);
    let backend = Simulated::new(config, Arc::new(WallClock))?;
    backend.restore(&state)?;
    let backend = Arc::new(backend);
    let safety_path = state.with_extension("safety.json");
    let safety =
        dutchman_print::safety::Safety::new(backend.clone()).with_store(safety_path.clone());
    safety.restore(&safety_path).await?;
    let result = command(args.command, &safety, &data_dir).await;
    safety.save(&safety_path).await?;
    backend.save(&state)?;
    result
}
fn state_path(data_dir: &Path, config: &Path) -> Result<PathBuf, String> {
    use std::hash::{Hash as _, Hasher as _};
    let config = std::fs::canonicalize(config).map_err(|_| "Cannot locate the config file.")?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    config.hash(&mut hash);
    Ok(data_dir.join(format!("simulation-{:016x}.json", hash.finish())))
}
fn output(value: &impl serde::Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string(value).map_err(|_| "Cannot write JSON output.")?
    );
    Ok(())
}
fn confirm(prompt: &str, yes: bool) -> Result<bool, String> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err("This action needs confirmation on a terminal. Use --yes to confirm on the command line.".into());
    }
    ask(
        &mut std::io::stdin().lock(),
        &mut std::io::stderr().lock(),
        prompt,
    )
}
fn ask(
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
    prompt: &str,
) -> Result<bool, String> {
    write!(output, "{prompt} [y/N] ").map_err(|_| "Cannot show the confirmation prompt.")?;
    output
        .flush()
        .map_err(|_| "Cannot show the confirmation prompt.")?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .map_err(|_| "Cannot read confirmation.")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

async fn command(command: Command, backend: &dyn Backend, data: &Path) -> Result<(), String> {
    let result: Result<(), dutchman_print::backend::Error> = async {
        match command {
            Command::Print {
                file,
                printer,
                printer_profile,
                print_profile,
                filament_profile,
                yes,
                json,
            } => {
                let id =
                    dutchman_print::new_job_id().map_err(dutchman_print::backend::Error::Failed)?;
                let name = dutchman_serve::upload::safe_name(
                    &file.file_name().unwrap_or_default().to_string_lossy(),
                );
                let dir = data.join(&id);
                let estimate = prepare_cli(
                    backend,
                    &id,
                    &file,
                    &printer,
                    Options {
                        name: Some(name.clone()),
                        printer_profile,
                        print_profile,
                        filament_profile,
                    },
                    &dir,
                )
                .await;
                if estimate.is_err() {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                }
                let estimate = estimate?;
                if !json {
                    eprintln!(
                        "Estimate: {} seconds, {} mm filament, {} g, {} layers.",
                        estimate
                            .seconds
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "unavailable".into()),
                        estimate
                            .filament_mm
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "unavailable".into()),
                        estimate
                            .filament_g
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "unavailable".into()),
                        estimate
                            .layers
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "unavailable".into())
                    );
                }
                let start = if json {
                    yes
                } else {
                    confirm(&format!("Start printing {name} on {printer}?"), yes)
                        .map_err(dutchman_print::backend::Error::Refused)?
                };
                let job = if start {
                    let job = backend.start(&id).await?;
                    dutchman_print::log_action("start", &job);
                    job
                } else {
                    backend.job(&id).await?
                };
                output(&job).map_err(dutchman_print::backend::Error::Failed)?;
            }
            Command::Status { job } => {
                output(&backend.job(&job).await?)
                    .map_err(dutchman_print::backend::Error::Failed)?;
            }
            Command::Cancel { job, yes } => {
                let target = backend.job(&job).await?;
                if confirm(
                    &format!("Cancel printing {} on {}?", target.name, target.printer),
                    yes,
                )
                .map_err(dutchman_print::backend::Error::Refused)?
                {
                    let job = backend.cancel(&job).await?;
                    dutchman_print::log_action("cancel", &job);
                    output(&job).map_err(dutchman_print::backend::Error::Failed)?;
                }
            }
            Command::Clear { printer, yes } => {
                if confirm(&format!("Is the bed of {printer} clear?"), yes)
                    .map_err(dutchman_print::backend::Error::Refused)?
                {
                    backend.clear(&printer).await?;
                    dutchman_print::log_clear(&printer);
                }
            }
            Command::Pause { printer, yes } => {
                if !confirm(&format!("Pause printing on {printer}?"), yes)
                    .map_err(dutchman_print::backend::Error::Refused)?
                {
                    return Ok(());
                }
                let job = backend.pause(&printer).await?;
                dutchman_print::log_action("pause", &job);
                output(&job).map_err(dutchman_print::backend::Error::Failed)?;
            }
            Command::Resume { printer, yes } => {
                if !confirm(&format!("Resume printing on {printer}?"), yes)
                    .map_err(dutchman_print::backend::Error::Refused)?
                {
                    return Ok(());
                }
                let job = backend.resume(&printer).await?;
                dutchman_print::log_action("resume", &job);
                output(&job).map_err(dutchman_print::backend::Error::Failed)?;
            }
            Command::Printers { .. } | Command::Serve { .. } => {}
        }
        Ok(())
    }
    .await;
    result.map_err(|err| err.to_string())
}
async fn prepare_cli(
    backend: &dyn Backend,
    id: &str,
    file: &Path,
    printer: &str,
    options: Options,
    dir: &Path,
) -> Result<dutchman_print::backend::Estimate, dutchman_print::backend::Error> {
    use dutchman_print::backend::Error;
    options.validate().map_err(Error::Refused)?;
    let settings = dutchman_print::StorageConfig::default();
    if !file
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("3mf"))
    {
        return Err(Error::Refused("Choose a .3mf file.".into()));
    }
    let metadata = tokio::fs::metadata(file)
        .await
        .map_err(|_| Error::Failed("Cannot read the 3MF file.".into()))?;
    if metadata.len() > settings.max_upload {
        return Err(Error::Refused("The 3MF file is too big.".into()));
    }
    hardening_storage(
        dir.parent().unwrap_or(dir),
        settings.max_data,
        settings.min_free,
    )?;
    tokio::fs::create_dir(dir)
        .await
        .map_err(|_| Error::Failed("Cannot create a job folder.".into()))?;
    let input = dir.join("input.3mf");
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let source = tokio::fs::File::open(file)
        .await
        .map_err(|_| Error::Failed("Cannot read the 3MF file.".into()))?;
    let mut target = tokio::fs::File::create(&input)
        .await
        .map_err(|_| Error::Failed("Cannot keep the 3MF file.".into()))?;
    let copied = tokio::io::copy(
        &mut source.take(settings.max_upload.saturating_add(1)),
        &mut target,
    )
    .await
    .map_err(|_| Error::Failed("Cannot keep the 3MF file.".into()))?;
    if copied > settings.max_upload {
        return Err(Error::Refused("The 3MF file is too big.".into()));
    }
    target
        .flush()
        .await
        .map_err(|_| Error::Failed("Cannot keep the 3MF file.".into()))?;
    drop(target);
    let checked = input.clone();
    let scratch = dir.join("check");
    tokio::task::spawn_blocking(move || {
        dutchman_print::upload::check_3mf(&checked, &scratch, settings.zip)
    })
    .await
    .map_err(|_| Error::Failed("Cannot check the 3MF file.".into()))?
    .map_err(Error::Refused)?;
    backend.prepare(id, &input, printer, options).await
}
fn hardening_storage(
    dir: &Path,
    max: u64,
    free: u64,
) -> Result<(), dutchman_print::backend::Error> {
    dutchman_serve::hardening::check_storage(dir, max, free).map_err(|_| {
        dutchman_print::backend::Error::Failed(
            "Too little job storage. Remove old simulated jobs.".into(),
        )
    })
}
async fn stopped() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { () = interrupt => {} () = terminate => {} }
}

async fn real_commands(
    cmd: Command,
    config: config::Config,
    data: &Path,
    config_path: &Path,
) -> Result<(), String> {
    let state = state_path(data, config_path)?.with_extension("printers.json");
    let backend = dutchman_print::printers::Printers::new(config)?;
    if let Command::Printers { json } = cmd {
        if data.exists() {
            dutchman_serve::prepare_data_dir(data)
                .map_err(|_| "Cannot use the private data folder.")?;
        }
        backend.restore(&state).await?;
        let safety = dutchman_print::safety::Safety::new(Arc::new(backend));
        safety.restore(&state.with_extension("safety.json")).await?;
        let reports = safety.printers().await.map_err(|e| e.to_string())?;
        if json {
            output(&reports)?;
        } else {
            print!("{}", dutchman_print::text_report(&reports));
        }
        return Ok(());
    }
    dutchman_serve::prepare_data_dir(data).map_err(|_| "Cannot use the private data folder.")?;
    if let Command::Serve {
        listen,
        user,
        public_origin,
    } = cmd
    {
        let login = std::env::var("DUTCHMAN_PRINT_PASSWORD")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|password| (user, password));
        if login.is_none() && !listen.ip().is_loopback() {
            eprintln!("dutchman-print: set DUTCHMAN_PRINT_PASSWORD to protect machine actions");
        }
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|_| "Cannot listen on the selected address.")?;
        let mut settings = ServerConfig {
            login,
            public_origin,
            ..Default::default()
        };
        settings.storage.data_dir = data.into();
        let cleanup = tokio::spawn(dutchman_print::remove_expired(
            data.into(),
            settings.storage.keep,
        ));
        let result = axum::serve(
            listener,
            dutchman_print::router(Arc::new(backend), settings),
        )
        .with_graceful_shutdown(stopped())
        .await;
        cleanup.abort();
        return result.map_err(|_| "Print server stopped unexpectedly.".into());
    }
    let lock = state.with_extension("lock");
    std::fs::create_dir(&lock)
        .map_err(|_| "Another CLI command is using these printers. Wait for it to finish.")?;
    let _lock = Lock(lock);
    let backend = Arc::new(backend.with_store(state.clone()));
    backend.restore(&state).await?;
    let safety_path = state.with_extension("safety.json");
    let safety =
        dutchman_print::safety::Safety::new(backend.clone()).with_store(safety_path.clone());
    safety.restore(&safety_path).await?;
    let result = command(cmd, &safety, data).await;
    backend.save(&state).await?;
    safety.save(&safety_path).await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_defaults_to_no_and_names_the_job_and_printer() {
        for (answer, expected) in [
            ("", false),
            ("\n", false),
            ("n\n", false),
            ("no\n", false),
            ("y\n", true),
            ("YES\n", true),
            ("anything\n", false),
        ] {
            let mut input = std::io::Cursor::new(answer);
            let mut output = Vec::new();
            assert_eq!(
                ask(&mut input, &mut output, "Start printing part.3mf on demo?").unwrap(),
                expected
            );
            assert_eq!(output, b"Start printing part.3mf on demo? [y/N] ");
        }
    }
}
