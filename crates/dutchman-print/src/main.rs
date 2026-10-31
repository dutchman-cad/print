//! Command line for the printer companion.

use std::io::IsTerminal as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::{Parser, Subcommand};
use dutchman_print::backend::{Backend, Options};
use dutchman_print::simulated::{Clock, Simulated};
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
    /// List persistent finished-job and bed-clear history.
    History {
        #[command(flatten)]
        filter: dutchman_print::history::Filter,
        #[arg(long)]
        json: bool,
        /// Show counts, success rate and print time per printer instead of records.
        #[arg(long, conflicts_with = "csv")]
        summary: bool,
        /// Write the records as CSV with a header row.
        #[arg(long, conflicts_with = "json")]
        csv: bool,
    },
    /// Write a config for one printer, asking on a terminal. Contacts nothing.
    Init(Box<dutchman_print::init::Flags>),
    /// Manage the running server's print farm queue.
    Queue(queue_cli::Flags),
    /// List printers.
    Printers {
        #[arg(long)]
        json: bool,
    },
    /// Read a Bambu TLS certificate without MQTT login or changing config.
    Fingerprint {
        printer: String,
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
        /// Acknowledge the checked, clear bed in this run.
        #[arg(long)]
        bed_clear: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show a tracked CLI job.
    Status { job: String },
    /// Cancel a job after confirmation.
    Cancel {
        job: String,
        #[arg(long)]
        yes: bool,
    },
    /// Stop the printer's live job after confirmation.
    Stop {
        printer: String,
        #[arg(long)]
        yes: bool,
    },
    /// Show bed acknowledgement status; clearance is kept only by a running server.
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
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(2..=30))]
        clearance_interval_seconds: u64,
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=120))]
        clearance_expiry_minutes: u64,
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=600))]
        confirmation_seconds: u64,
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
    if let Command::Init(flags) = args.command {
        return dutchman_print::init::run(*flags, args.config);
    }
    if let Command::Queue(flags) = args.command {
        return queue_cli::run(flags).await;
    }
    let path = args.config.map(Ok).unwrap_or_else(config::default_path)?;
    if let Command::Fingerprint { printer, json } = &args.command {
        let config = config::Config::load_for_fingerprint(&path, printer)?;
        let p = config
            .printers
            .iter()
            .find(|p| &p.id == printer)
            .ok_or("Choose a configured Bambu printer id.")?;
        if config.backend.kind != "printers" || p.kind.as_deref() != Some("bambu") {
            return Err(
                "Fingerprint requires a configured Bambu printer in the printers backend.".into(),
            );
        }
        let report = dutchman_print::bambu::fingerprint(p).await?;
        if *json {
            output(&report)?;
        } else {
            println!(
                "SHA-256: {}\nSubject: {}\nIssuer: {}\nValid from: {}\nValid until: {}\n{}",
                report.cert_sha256,
                report.subject,
                report.issuer,
                report.not_before,
                report.not_after,
                report.instruction
            );
        }
        return Ok(());
    }
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
        let _lock = if data_dir.exists() {
            let path = state.with_extension("lock");
            std::fs::create_dir(&path).map_err(|_| {
                "Another CLI command is using simulated jobs. Wait for it to finish."
            })?;
            Some(Lock(path))
        } else {
            None
        };
        backend.restore_for_history(&state)?;
        let backend = Arc::new(backend);
        let mut safety = dutchman_print::safety::Safety::new(backend.clone());
        let safety_path = state.with_extension("safety.json");
        if data_dir.exists() {
            safety = safety
                .with_store(safety_path.clone())
                .with_history(data_dir.clone());
        }
        safety.restore(&safety_path).await?;
        safety.poll_history_best_effort(true).await;
        let reports = safety.printers().await;
        if data_dir.exists() {
            safety.poll_history_best_effort(false).await;
            safety.flush_history().await;
            backend.save(&state)?;
            safety.save(&safety_path).await?;
        }
        shutdown_backend(&safety).await;
        let reports = reports.map_err(|e| e.to_string())?;
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
        clearance_interval_seconds,
        clearance_expiry_minutes,
        confirmation_seconds,
    } = args.command
    {
        let _server_lock = server_lock(&data_dir)?;
        let backend = Arc::new(Simulated::new(config, Arc::new(WallClock))?);
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
            clearance_interval: Duration::from_secs(clearance_interval_seconds),
            clearance_expiry: Duration::from_secs(clearance_expiry_minutes * 60),
            confirmation_ttl: Duration::from_secs(confirmation_seconds),
            ..Default::default()
        };
        settings.storage.data_dir = data_dir.clone();
        eprintln!("dutchman-print: listening on http://{address}");
        let result = serve_until(
            listener,
            dutchman_print::router(backend.clone(), settings),
            backend.as_ref(),
            stopped(),
        )
        .await;
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
    backend.restore_for_history(&state)?;
    let backend = Arc::new(backend);
    let safety_path = state.with_extension("safety.json");
    let safety = dutchman_print::safety::Safety::new(backend.clone())
        .with_store(safety_path.clone())
        .with_history(data_dir.clone());
    safety.restore(&safety_path).await?;
    safety.poll_history_best_effort(true).await;
    let result = command(args.command, &safety, &data_dir).await;
    safety.poll_history_best_effort(false).await;
    safety.flush_history().await;
    let safety_saved = safety.save(&safety_path).await;
    let jobs_saved = backend.save(&state);
    shutdown_backend(&safety).await;
    safety_saved?;
    jobs_saved?;
    result
}
fn server_lock(data: &Path) -> Result<Lock, String> {
    let path = data.join("serve.lock");
    std::fs::create_dir(&path).map_err(|_| "Another server is using this data folder. Stop it first; remove serve.lock only after it has stopped.")?;
    Ok(Lock(path))
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

async fn confirm_action(
    backend: &dutchman_print::safety::Safety,
    confirmations: &dutchman_print::confirmation::Confirmations,
    action: &str,
    printer: &str,
    job: Option<&str>,
    yes: bool,
) -> Result<bool, dutchman_print::backend::Error> {
    let intent = dutchman_print::confirmation::Intent {
        action: action.into(),
        printer: printer.into(),
        job: job.map(str::to_owned),
    };
    let challenge = confirmations
        .issue(
            backend,
            intent.clone(),
            WallClock.now(),
            Duration::from_secs(60),
        )
        .await?;
    if yes {
        eprintln!("{}", challenge.summary);
    }
    if !confirm(&challenge.summary, yes).map_err(dutchman_print::backend::Error::Refused)? {
        return Ok(false);
    }
    confirmations
        .consume(backend, &challenge.id, &intent, None, WallClock.now())
        .await?;
    Ok(true)
}

async fn command(
    command: Command,
    backend: &dutchman_print::safety::Safety,
    data: &Path,
) -> Result<(), String> {
    let confirmations = dutchman_print::confirmation::Confirmations::new(None);
    let result: Result<(), dutchman_print::backend::Error> = async {
        match command {
            Command::History { filter, json, summary, csv } => {
                filter.validate().map_err(dutchman_print::backend::Error::Refused)?;
                backend.flush_history().await;
                backend.poll_history(false).await?;
                if summary {
                    if filter.outcome.is_some() || filter.limit.is_some() {
                        return Err(dutchman_print::backend::Error::Refused(
                            "A summary covers every outcome and record; drop --outcome and --limit.".into()));
                    }
                    let filter = dutchman_print::history::SummaryFilter { printer: filter.printer, since: filter.since };
                    let summary = dutchman_print::history::summary(data, &filter).await?;
                    if json { output(&summary).map_err(dutchman_print::backend::Error::Failed)?; } else { println!("{}", summary.table()); }
                } else {
                let records = dutchman_print::history::query(data, &filter).await?;
                if csv {
                    print!("{}", dutchman_print::history::csv(&records));
                } else if json { output(&records).map_err(dutchman_print::backend::Error::Failed)?; } else {
                    for record in records {
                        println!("{} {} {} {}", record.ended_at, record.printer, record.outcome,
                            record.job_name.as_deref().unwrap_or("Bed clear"));
                    }
                }
                }
            },
            Command::Print {
                file,
                printer,
                printer_profile,
                print_profile,
                filament_profile,
                bed_clear,
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
                backend.history_source(&id, &file.file_name().unwrap_or_default().to_string_lossy()).await?;
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
                let start = if json && !yes { false } else {
                    if yes && !bed_clear {
                        return Err(dutchman_print::backend::Error::Refused("Use --bed-clear --yes after checking the bed, or confirm interactively.".into()));
                    }
                    if !confirm_action(backend, &confirmations, "clear", &printer, None, yes && bed_clear).await? { return Ok(()) }
                    backend.clear(&printer).await?;
                    confirm_action(backend, &confirmations, "start", &printer, Some(&id), yes).await?
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
                if confirm_action(backend, &confirmations, "cancel", &target.printer, Some(&job), yes).await?
                {
                    let job = backend.cancel(&job).await?;
                    dutchman_print::log_action("cancel", &job);
                    output(&job).map_err(dutchman_print::backend::Error::Failed)?;
                }
            }
            Command::Stop { printer, yes } => {
                let intent = dutchman_print::confirmation::Intent { action: "stop".into(), printer: printer.clone(), job: None };
                let challenge = confirmations.issue(backend, intent.clone(), WallClock.now(), Duration::from_secs(60)).await?;
                if yes { eprintln!("{}", challenge.summary); }
                if confirm(&challenge.summary, yes).map_err(dutchman_print::backend::Error::Refused)? {
                    let observed = confirmations.consume(backend, &challenge.id, &intent, None, WallClock.now()).await?;
                    let identity = observed.job_id.as_deref().ok_or(dutchman_print::backend::Error::Unavailable)?;
                    let name = observed.job_name.as_deref().ok_or(dutchman_print::backend::Error::Unavailable)?;
                    output(&backend.stop(&printer, identity, name, observed.job_path.as_deref()).await?).map_err(dutchman_print::backend::Error::Failed)?;
                }
            }
            Command::Clear { printer, yes: _ } => {
                let challenge = confirmations.issue(backend, dutchman_print::confirmation::Intent { action: "clear".into(), printer, job: None }, WallClock.now(), Duration::from_secs(60)).await?;
                println!("{}\nClearance is only kept by a running server. Use print with --bed-clear --yes or confirm both questions interactively.", challenge.summary);
            }
            Command::Pause { printer, yes } => {
                if !confirm_action(backend, &confirmations, "pause", &printer, None, yes).await?
                {
                    return Ok(());
                }
                let job = backend.pause(&printer).await?;
                dutchman_print::log_action("pause", &job);
                output(&job).map_err(dutchman_print::backend::Error::Failed)?;
            }
            Command::Resume { printer, yes } => {
                if !confirm_action(backend, &confirmations, "resume", &printer, None, yes).await?
                {
                    return Ok(());
                }
                let job = backend.resume(&printer).await?;
                dutchman_print::log_action("resume", &job);
                output(&job).map_err(dutchman_print::backend::Error::Failed)?;
            }
            Command::Init(_)
            | Command::Queue(_)
            | Command::Printers { .. }
            | Command::Serve { .. }
            | Command::Fingerprint { .. } => {}
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
    let backend = if matches!(cmd, Command::Serve { .. }) {
        dutchman_print::printers::Printers::new(config)?
    } else {
        dutchman_print::printers::Printers::one_shot(config)?
    };
    let store = if matches!(cmd, Command::Serve { .. }) {
        data.join("jobs.json")
    } else {
        state.clone()
    };
    let backend = Arc::new(if matches!(cmd, Command::Printers { .. }) {
        backend
    } else {
        backend.with_store(store)
    });
    let serving = matches!(cmd, Command::Serve { .. });
    // Keep the CLI lock through cleanup, including older Duet IP-based authentication.
    let _lock = if !serving && !matches!(cmd, Command::Printers { .. }) {
        dutchman_serve::prepare_data_dir(data)
            .map_err(|_| "Cannot use the private data folder.")?;
        let lock = state.with_extension("lock");
        std::fs::create_dir(&lock)
            .map_err(|_| "Another CLI command is using these printers. Wait for it to finish.")?;
        Some(Lock(lock))
    } else {
        None
    };
    let result = real_command(cmd, backend.clone(), data, &state).await;
    if !serving {
        shutdown_backend(backend.as_ref()).await;
    }
    result
}

async fn shutdown_backend(backend: &dyn Backend) {
    // Never display backend errors: they could include credentials or private addresses.
    let _ = tokio::time::timeout(Duration::from_secs(3), backend.shutdown()).await;
}

async fn serve_until(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    backend: &dyn Backend,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), std::io::Error> {
    use std::future::IntoFuture as _;
    let (signal, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .into_future();
    tokio::pin!(server);
    let result = tokio::select! {
        result = &mut server => Some(result),
        () = stop => None,
    };
    let _ = signal.send(());
    // Durable start attempts and existing job/safety checkpoints remain intact.
    // Drain HTTP requests alongside disconnects, sharing one overall deadline.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(shutdown_backend(backend), async {
            if result.is_none() {
                let _ = (&mut server).await;
            }
        });
    })
    .await;
    result.unwrap_or(Ok(()))
}

async fn real_command(
    cmd: Command,
    backend: Arc<dutchman_print::printers::Printers>,
    data: &Path,
    state: &Path,
) -> Result<(), String> {
    if let Command::Printers { json } = cmd {
        if data.exists() {
            dutchman_serve::prepare_data_dir(data)
                .map_err(|_| "Cannot use the private data folder.")?;
        }
        let _lock = if data.exists() {
            let path = state.with_extension("lock");
            std::fs::create_dir(&path).map_err(|_| {
                "Another CLI command is using these printers. Wait for it to finish."
            })?;
            Some(Lock(path))
        } else {
            None
        };
        backend.restore(state).await?;
        let mut safety = dutchman_print::safety::Safety::new(backend.clone());
        let safety_path = state.with_extension("safety.json");
        if data.exists() {
            safety = safety
                .with_store(safety_path.clone())
                .with_history(data.into());
        }
        safety.restore(&safety_path).await?;
        safety.poll_history_best_effort(true).await;
        let reports = safety.printers().await.map_err(|e| e.to_string())?;
        if data.exists() {
            safety.poll_history_best_effort(false).await;
            safety.flush_history().await;
            backend.save(state).await?;
            safety.save(&safety_path).await?;
        }
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
        clearance_interval_seconds,
        clearance_expiry_minutes,
        confirmation_seconds,
    } = cmd
    {
        let _server_lock = server_lock(data)?;
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
            clearance_interval: Duration::from_secs(clearance_interval_seconds),
            clearance_expiry: Duration::from_secs(clearance_expiry_minutes * 60),
            confirmation_ttl: Duration::from_secs(confirmation_seconds),
            ..Default::default()
        };
        settings.storage.data_dir = data.into();
        let result = serve_until(
            listener,
            dutchman_print::router(backend.clone(), settings),
            backend.as_ref(),
            stopped(),
        )
        .await;
        return result.map_err(|_| "Print server stopped unexpectedly.".into());
    }
    backend.restore(state).await?;
    let safety_path = state.with_extension("safety.json");
    let safety = dutchman_print::safety::Safety::new(backend.clone())
        .with_store(safety_path.clone())
        .with_history(data.into());
    safety.restore(&safety_path).await?;
    safety.poll_history_best_effort(true).await;
    let result = command(cmd, &safety, data).await;
    let jobs_saved = backend.save(state).await;
    safety.poll_history_best_effort(false).await;
    safety.flush_history().await;
    let safety_saved = safety.save(&safety_path).await;
    jobs_saved?;
    safety_saved?;
    result
}

#[cfg(test)]
#[path = "../tests/helpers/snapmaker.rs"]
mod shutdown_fake;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn server_stop_disconnects_once_even_with_a_pending_request() {
        use tokio::io::AsyncWriteExt as _;
        let fake = shutdown_fake::Fake2::new(shutdown_fake::Approval::Immediate).await;
        let backend = Arc::new(dutchman_print::printers::Printers::new(fake.config()).unwrap());
        backend.printers().await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let app = axum::Router::new().route(
            "/pending",
            axum::routing::get({
                let entered = entered.clone();
                move || {
                    let entered = entered.clone();
                    async move {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                    }
                }
            }),
        );
        let (stop, signal) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            serve_until(listener, app, backend.as_ref(), async {
                let _ = signal.await;
            })
            .await
        });
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /pending HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        let started = std::time::Instant::now();
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(3500), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(3500));
        assert_eq!(fake.count("/api/v1/disconnect").await, 1);
    }

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

mod queue_cli;
